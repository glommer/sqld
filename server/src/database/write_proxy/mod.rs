mod replication;

use std::future::{ready, Ready};
use std::path::PathBuf;
#[cfg(feature = "mwal_backend")]
use std::sync::Arc;
use std::time::Duration;

use crossbeam::channel::TryRecvError;
use tokio::sync::Mutex;
use tonic::transport::Channel;
use uuid::Uuid;

use crate::query::{ErrorCode, QueryError, QueryResponse, QueryResult, Value};
use crate::query_analysis::{State, Statement};
use crate::rpc::proxy::proxy_rpc::proxy_client::ProxyClient;
use crate::rpc::proxy::proxy_rpc::{query_result, DisconnectMessage, SimpleQuery};

use super::{libsql::LibSqlDb, service::DbFactory, Database};
use replication::PeriodicDbUpdater;

#[derive(Clone)]
pub struct WriteProxyDbFactory {
    write_proxy: ProxyClient<Channel>,
    db_path: PathBuf,
    #[cfg(feature = "mwal_backend")]
    vwal_methods: Option<Arc<std::sync::Mutex<mwal::ffi::libsql_wal_methods>>>,
    /// abort handle: abort db update loop on drop
    _abort_handle: crossbeam::channel::Sender<()>,
}

impl WriteProxyDbFactory {
    pub async fn new(
        addr: &str,
        db_path: PathBuf,
        #[cfg(feature = "mwal_backend")] vwal_methods: Option<
            Arc<std::sync::Mutex<mwal::ffi::libsql_wal_methods>>,
        >,
    ) -> anyhow::Result<Self> {
        let write_proxy = ProxyClient::connect(addr.to_string()).await?;
        let mut db_updater =
            PeriodicDbUpdater::new(&db_path, addr.to_string(), Duration::from_secs(1)).await?;
        let (_abort_handle, receiver) = crossbeam::channel::bounded::<()>(1);
        tokio::task::spawn_blocking(move || loop {
            // must abort
            if let Err(TryRecvError::Disconnected) = receiver.try_recv() {
                break;
            }
            db_updater.step();
        });
        Ok(Self {
            write_proxy,
            db_path,
            #[cfg(feature = "mwal_backend")]
            vwal_methods,
            _abort_handle,
        })
    }
}

impl DbFactory for WriteProxyDbFactory {
    type Future = Ready<anyhow::Result<Self::Db>>;

    type Db = WriteProxyDatabase;

    fn create(&self) -> Self::Future {
        ready(WriteProxyDatabase::new(
            self.write_proxy.clone(),
            self.db_path.clone(),
            #[cfg(feature = "mwal_backend")]
            self.vwal_methods.clone(),
        ))
    }
}

pub struct WriteProxyDatabase {
    read_db: LibSqlDb,
    write_proxy: ProxyClient<Channel>,
    state: Mutex<State>,
    client_id: Uuid,
}

impl WriteProxyDatabase {
    fn new(
        write_proxy: ProxyClient<Channel>,
        path: PathBuf,
        #[cfg(feature = "mwal_backend")] vwal_methods: Option<
            Arc<std::sync::Mutex<mwal::ffi::libsql_wal_methods>>,
        >,
    ) -> anyhow::Result<Self> {
        let read_db = LibSqlDb::new(
            path,
            #[cfg(feature = "mwal_backend")]
            vwal_methods,
            (),
        )?;
        Ok(Self {
            read_db,
            write_proxy,
            state: Mutex::new(State::Start),
            client_id: Uuid::new_v4(),
        })
    }
}

#[async_trait::async_trait]
impl Database for WriteProxyDatabase {
    async fn execute(&self, query: Statement, params: Vec<Value>) -> QueryResult {
        let mut state = self.state.lock().await;
        if query.is_read_only() && *state == State::Start {
            self.read_db.execute(query, params).await
        } else {
            let mut next_state = *state;
            next_state.step(query.kind);
            let query = SimpleQuery {
                q: query.stmt,
                client_id: self.client_id.as_bytes().to_vec(),
            };
            let mut client = self.write_proxy.clone();
            match client.query(query).await {
                Ok(r) => {
                    let result = r.into_inner();
                    match result.result() {
                        query_result::Result::Ok => {
                            let rows = result.rows.expect("invalid response");
                            *state = next_state;
                            return Ok(QueryResponse::ResultSet(rows.into()));
                        }
                        // FIXME: correct error handling
                        query_result::Result::Err => Err(QueryError::from(result.error.unwrap())),
                    }
                }
                // state unknown!
                Err(e) => Err(QueryError::new(ErrorCode::Internal, e)),
            }
        }
    }
}

impl Drop for WriteProxyDatabase {
    fn drop(&mut self) {
        // best effort attempt to disconnect
        let mut remote = self.write_proxy.clone();
        let client_id = self.client_id.as_bytes().to_vec();
        tokio::spawn(async move {
            let _ = remote.disconnect(DisconnectMessage { client_id }).await;
        });
    }
}
