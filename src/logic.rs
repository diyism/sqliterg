// Copyright (c) 2023-, Germano Rizzo <oss /AT/ germanorizzo /DOT/ it>
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{collections::HashMap, ops::DerefMut};

use actix_web::{http::header::Header, web, HttpRequest, Responder};
use actix_web_httpauth::headers::authorization::{Authorization, Basic};
use eyre::Result;
use rusqlite::{types::Value, Connection, ToSql, Transaction};
use serde_json::{json, Map as JsonMap, Value as JsonValue};

use crate::{
    auth::process_auth,
    commons::{check_stored_stmt, prepend_colon, NamedParamsContainer},
    db_config::{AuthMode, DbConfig},
    main_config::Db,
    req_res::{self, Response, ResponseItem},
    MUTEXES,
};

fn val_db2val_json(val: Value) -> JsonValue {
    match val {
        Value::Null => JsonValue::Null,
        Value::Integer(v) => json!(v),
        Value::Real(v) => json!(v),
        Value::Text(v) => json!(v),
        Value::Blob(v) => json!(v),
    }
}

// adapted from serde-rusqlite, https://github.com/twistedfall/serde_rusqlite/blob/master/LICENSE
fn calc_named_params(params: &JsonMap<String, JsonValue>) -> NamedParamsContainer {
    let mut named_params: Vec<(String, Box<dyn ToSql>)> = Vec::new();

    params
        .iter()
        .for_each(|(k, v)| named_params.push((prepend_colon(k), Box::new(v.to_owned()))));

    NamedParamsContainer::from(named_params)
}

#[allow(clippy::type_complexity)]
fn do_query(
    tx: &Transaction,
    sql: &str,
    values: &Option<JsonValue>,
) -> Result<(Option<Vec<JsonValue>>, Option<usize>, Option<Vec<usize>>)> {
    let mut stmt = tx.prepare(sql)?;
    let column_names: Vec<String> = stmt
        .column_names()
        .iter()
        .map(|cn| cn.to_string())
        .collect();
    let mut rows = match values {
        Some(p) => {
            let map = p.as_object().unwrap();
            stmt.query(calc_named_params(map).slice().as_slice())?
        }
        None => stmt.query([])?,
    };
    let mut response = vec![];
    while let Some(row) = rows.next().unwrap() {
        let mut map: JsonMap<String, JsonValue> = JsonMap::new();
        for (i, col_name) in column_names.iter().enumerate() {
            let value: Value = row.get_unwrap(i);
            map.insert(col_name.to_string(), val_db2val_json(value));
        }
        response.push(JsonValue::Object(map));
    }
    Ok((Some(response), None, None))
}

#[allow(clippy::type_complexity)]
fn do_statement(
    tx: &Transaction,
    sql: &str,
    values: &Option<JsonValue>,
    values_batch: &Option<Vec<JsonValue>>,
) -> Result<(Option<Vec<JsonValue>>, Option<usize>, Option<Vec<usize>>)> {
    if values.is_some() && values_batch.is_some() {
        return Err(eyre!(
            "at most one of values and values_batch must be provided"
        ));
    }

    Ok(if values.is_none() && values_batch.is_none() {
        let changed_rows = tx.execute(sql, [])?;
        (None, Some(changed_rows), None)
    } else if values.is_some() {
        let map = values.as_ref().unwrap().as_object().unwrap();
        let changed_rows = tx.execute(sql, calc_named_params(map).slice().as_slice())?;
        (None, Some(changed_rows), None)
    } else {
        // values_batch.is_some()
        let mut stmt = tx.prepare(sql)?;
        let mut ret = vec![];
        for p in values_batch.as_ref().unwrap() {
            let map = p.as_object().unwrap();
            let changed_rows = stmt.execute(calc_named_params(map).slice().as_slice())?;
            ret.push(changed_rows);
        }
        (None, None, Some(ret))
    })
}

fn process(
    conn: &mut Connection,
    http_req: web::Json<req_res::Request>,
    stored_statements: &HashMap<String, String>,
    dbconf: &DbConfig,
    auth_header: &Option<Authorization<Basic>>,
) -> Result<Response> {
    if dbconf.auth.is_some()
        && !process_auth(
            dbconf.auth.as_ref().unwrap(),
            conn,
            &http_req.credentials,
            auth_header,
        )
    {
        return Ok(Response::new_err(
            401,
            -1,
            "Authorization failed".to_string(),
        ));
    }

    let tx = conn.transaction()?;

    let mut results = vec![];
    let mut failed = None;

    for (idx, trx_item) in http_req.transaction.iter().enumerate() {
        if trx_item.query.is_some() == trx_item.statement.is_some() {
            let msg = "exactly one of 'query' and 'statement' must be provided".to_string();
            if trx_item.no_fail {
                results.push(ResponseItem {
                    success: false,
                    error: Some(msg),
                    result_set: None,
                    rows_updated: None,
                    rows_updated_batch: None,
                });
                continue;
            } else {
                failed = Some((idx, msg));
                break;
            }
        }

        let ret = if let Some(query) = &trx_item.query {
            let sql =
                check_stored_stmt(query, stored_statements, dbconf.use_only_stored_statements);
            match sql {
                Ok(sql) => do_query(&tx, sql, &trx_item.values),
                Err(e) => Result::Err(e),
            }
        } else {
            let statement = trx_item.statement.as_ref().unwrap();
            let sql = check_stored_stmt(
                statement,
                stored_statements,
                dbconf.use_only_stored_statements,
            );
            match sql {
                Ok(sql) => do_statement(&tx, sql, &trx_item.values, &trx_item.values_batch),
                Err(e) => Result::Err(e),
            }
        };

        if !trx_item.no_fail {
            if let Err(err) = ret {
                failed = Some((idx, err.to_string()));
                break;
            }
        }

        results.push(match ret {
            Ok(val) => ResponseItem {
                success: true,
                error: None,
                result_set: val.0,
                rows_updated: val.1,
                rows_updated_batch: val.2,
            },
            Err(err) => ResponseItem {
                success: false,
                error: Some(err.to_string()),
                result_set: None,
                rows_updated: None,
                rows_updated_batch: None,
            },
        });
    }

    Ok(match failed {
        Some(f) => {
            tx.rollback()?;
            // FIXME should be 500 for SQL errors, though...
            Response::new_err(400, f.0 as isize, f.1)
        }
        None => {
            tx.commit()?;
            Response::new_ok(results)
        }
    })
}

pub async fn handler(
    req: HttpRequest,
    body: web::Json<req_res::Request>,
    db_conf: web::Data<Db>,
    db_name: web::Data<String>,
) -> impl Responder {
    let auth = if (db_conf).conf.auth.is_some()
        && matches!(
            db_conf.conf.auth.as_ref().unwrap().mode,
            AuthMode::HttpBasic
        ) {
        match Authorization::<Basic>::parse(&req) {
            Ok(a) => Some(a),
            Err(_) => None,
        }
    } else {
        None
    };

    let db_lock = MUTEXES.get().unwrap().get(&db_name.to_string()).unwrap();
    let mut db_lock_guard = db_lock.lock().unwrap();
    let conn = db_lock_guard.deref_mut();

    process(conn, body, &db_conf.stored_statements, &db_conf.conf, &auth).unwrap()
}
