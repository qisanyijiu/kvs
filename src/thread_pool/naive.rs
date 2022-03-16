use std::thread;

use super::ThreadPool;
use crate::Result;

#[derive(Clone)]
pub struct NaiveThreadPool;

impl ThreadPool for NaiveThreadPool {
    fn new(_threads: u32) -> Result<Self> {
        Ok(NaiveThreadPool)
    }

    fn spawn<F>(&self, f: F)
    where F: FnOnce() + Send  + 'static
    {
        thread::spawn(f);
    }
}