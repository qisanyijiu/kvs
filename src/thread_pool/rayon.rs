use super::ThreadPool;
use crate::{KvsError, Result};
use std::sync::Arc;

#[derive(Clone)]
pub struct RayonThreadPool(Arc<rayon::ThreadPool>);

impl ThreadPool for RayonThreadPool {
    fn new(_threads: u32) -> Result<Self> {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(_threads as usize)
            .build()
            .map_err(|e| KvsError::StringError(format!("{}", e)))?;

        Ok(RayonThreadPool(Arc::new(pool)))
    }

    fn spawn<F>(&self, f: F) 
    where F: FnOnce() + Send + 'static
    {
        self.0.spawn(f);
    } 
}