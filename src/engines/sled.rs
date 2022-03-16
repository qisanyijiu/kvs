use crate::thread_pool::ThreadPool;
use crate::{KvsEngine, KvsError, Result};
use sled::Db;
use tokio::prelude::*;
use tokio::sync::oneshot;

#[derive(Clone)]
pub struct SledKvsEngine<P: ThreadPool> {
    pool: P,
    db: Db,
}

impl<P: ThreadPool> SledKvsEngine<P> {
    pub fn new(db: Db, concurrency: u32) -> Result<Self> {
        let pool = P::new(concurrency)?;
        Ok(SledKvsEngine { pool, db})
    }
}

impl<P: ThreadPool> KvsEngine for SledKvsEngine<P> {
    fn set(&self, key: String, value: String) -> Box<dyn Future<Item = (), Error = KvsError> + Send> {
        let db = self.db.clone();
        let (tx, rx) = oneshot::channel();
        self.pool.spawn(move || {
            let res = db
            .set(key, value.into_bytes())
            .and_then(|_| db.flush())
            .map(| _ | ())
            .map_err(KvsError::from);
            if tx.send(res).is_err() {
                error!("Receving end is dropped")
            }
        });
        Box::new(
            rx.map_err(|e| KvsError::StringError(format!("{}", e))).flatten(),
        )
    }

    fn get(&self, key: String) -> Box<dyn Future<Item = Option<String>, Error = KvsError> + Send> {
        let db = self.db.clone();
        let (tx, rx) = oneshot::channel();
        self.pool.spawn(move || {
            let res = (move || {
                Ok(db
                    .get(key)?
                    .map(|i_vec| AsRef::<[u8]>::as_ref(&i_vec).to_vec())
                    .map(String::from_utf8)
                    .transpose()?
                )
            })();
            if tx.send(res).is_err() {
                error!("Receving end is dropped");
            }
        });
        Box::new(
            rx.map_err(|e| KvsError::StringError(format!("{}", e))).flatten(),
        )
    }

    fn remove(&self, key: String) -> Box<dyn Future<Item = (), Error = KvsError> + Send> {
        let db = self.db.clone();
        let (tx, rx) = oneshot::channel();
        self.pool.spawn(move || {
            let res = (|| {
                db.del(key)?.ok_or(KvsError::KeyNotFount)?;
                db.flush();
                Ok(())
            })();
            if tx.send(res).is_err() {
                error!("Receving end is dropped")
            }
        });
        Box::new(
            rx.map_err(|e| KvsError::StringError(format!("{}", e))).flatten()
        )
    }
}