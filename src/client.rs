use crate::{common::*, tcp_pool::TcpPool};

use crate::reqs::*;
use async_net::TcpStream;

use dashmap::DashMap;
use lazy_static::lazy_static;

use serde::{de::DeserializeOwned, Serialize};
use smol::lock::Semaphore;
use smol_timeout::TimeoutExt;

use std::time::{Duration, Instant};
use std::{net::SocketAddr, sync::Arc};

lazy_static! {
    static ref CONN_POOL: Client = Client::default();
}

/// Does a melnet request to any given endpoint, using the global client.
pub async fn request<TInput: Serialize + Clone, TOutput: DeserializeOwned + std::fmt::Debug>(
    addr: SocketAddr,
    netname: &str,
    verb: &str,
    req: TInput,
) -> Result<TOutput> {
    match CONN_POOL
        .request(addr, netname, verb, req)
        .timeout(Duration::from_secs(60))
        .await
    {
        Some(v) => v,
        None => Err(MelnetError::Network(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "long timeout at 60 seconds",
        ))),
    }
}

/// Implements a thread-safe pool of connections to melnet, or any HTTP/1.1-style keepalive protocol, servers.
#[derive(Default)]
pub struct Client {
    pool: DashMap<SocketAddr, Arc<TcpPool>>,
}

impl Client {
    /// Does a melnet request to any given endpoint.
    pub async fn request<TInput: Serialize + Clone, TOutput: DeserializeOwned + std::fmt::Debug>(
        &self,
        addr: SocketAddr,
        netname: &str,
        verb: &str,
        req: TInput,
    ) -> Result<TOutput> {
        for count in 0..5 {
            match self.request_inner(addr, netname, verb, req.clone()).await {
                Err(MelnetError::Network(err)) => {
                    log::debug!(
                        "retrying request {} to {} on transient network error {:?}",
                        verb,
                        addr,
                        err
                    );
                    smol::Timer::after(Duration::from_secs_f64(0.1 * 2.0f64.powi(count))).await;
                }
                x => return x,
            }
        }
        self.request_inner(addr, netname, verb, req).await
    }

    async fn request_inner<TInput: Serialize, TOutput: DeserializeOwned + std::fmt::Debug>(
        &self,
        addr: SocketAddr,
        netname: &str,
        verb: &str,
        req: TInput,
    ) -> Result<TOutput> {
        // // Semaphore
        static GLOBAL_LIMIT: Semaphore = Semaphore::new(128);
        let _guard = GLOBAL_LIMIT.acquire().await;
        let start = Instant::now();
        let pool = self
            .pool
            .entry(addr)
            .or_insert_with(|| TcpPool::new(32, Duration::from_secs(5), addr).into())
            .clone();
        // grab a connection
        let mut conn = pool.connect().await.map_err(MelnetError::Network)?;

        let res = async {
            // send a request
            let rr = stdcode::serialize(&RawRequest {
                proto_ver: PROTO_VER,
                netname: netname.to_owned(),
                verb: verb.to_owned(),
                payload: stdcode::serialize(&req).unwrap(),
            })
            .unwrap();
            write_len_bts(&mut conn, &rr).await?;
            // read the response length
            let response: RawResponse = stdcode::deserialize(&read_len_bts(&mut conn).await?)
                .map_err(|e| {
                    MelnetError::Network(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
                })?;
            let response = match response.kind.as_ref() {
                "Ok" => stdcode::deserialize::<TOutput>(&response.body)
                    .map_err(|_| MelnetError::Custom("stdcode error".to_owned()))?,
                "NoVerb" => return Err(MelnetError::VerbNotFound),
                _ => {
                    return Err(MelnetError::Custom(
                        String::from_utf8_lossy(&response.body).to_string(),
                    ))
                }
            };
            let elapsed = start.elapsed();
            if elapsed.as_secs_f64() > 3.0 {
                log::warn!(
                    "melnet req of verb {}/{} to {} took {:?}",
                    netname,
                    verb,
                    addr,
                    elapsed
                )
            }
            self.pool.get(&addr).unwrap().replenish(conn);
            Ok::<_, crate::MelnetError>(response)
        };
        res.await
    }
}
