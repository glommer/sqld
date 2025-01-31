import fetch from 'cross-fetch';
import { ResultSet } from "../libsql-js";
import { Driver } from "./Driver";

export class HttpDriver implements Driver {
    url: URL;

    constructor(url: URL) {
        this.url = url;
    }

    async transaction(sql: string[]): Promise<ResultSet[]> {
        const query = {
            statements: sql
        };
        const response = await fetch(this.url, {
            method: 'POST',
            body: JSON.stringify(query),
        });
        const results = await response.json();
        // FIXME: Fix return value when there are multiple statements.
        return [{
            results: results,
            success: true,
            meta: {
                duration: 0,
            },
        }];
    }

}
