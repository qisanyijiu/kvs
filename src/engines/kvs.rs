use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write, Seek, SeekFrom};
use std::ops::Range;
use std::path::{PathBuf, Path};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crossbeam::queue::ArrayQueue;
use crossbeam_skiplist::SkipMap;
use serde::{Deserialize, Serialize};
use serde_json::Deserializer;
use tokio::prelude::*;
use tokio::sync::oneshot;

use super::KvsEngine;
use crate::thread_pool::ThreadPool;
use crate::{KvsError, Result};

const COMPACTION_THRESHOLD: u64 = 1024 * 1024;
/// The `KvStore` stores string key/value pairs.
///
/// Key/value pairs are persisted to disk in log files. Log files are named after
/// monotonically increasing generation numbers with a `log` extension name.
/// A skip list in memory stores the keys and the value locations for fast query.
///
/// ```rust
/// # use kvs::{KvStore, Result};
/// # use kvs::thread_pool::{ThreadPool, RayonThreadPool};
/// # use tokio::prelude::*;
/// # fn try_main() -> Result<()> {
/// use std::env::current_dir;
/// use kvs::KvsEngine;
/// let mut store: KvStore<RayonThreadPool> = KvStore::open(current_dir()?, 2)?;
/// store.set("key".to_owned(), "value".to_owned()).wait()?;
/// let val = store.get("key".to_owned()).wait()?;
/// assert_eq!(val, Some("value".to_owned()));
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct KvStore<P: ThreadPool> {
    // directory for the log and other data
    path: Arc<PathBuf>,
    index: Arc<SkipMap<String, CommandPos>>,
    writer: Arc<Mutex<KvStoreWriter>>,
    thread_pool: P,
    reader_pool: Arc<ArrayQueue<KvStoreReader>>,
}

impl<P: ThreadPool> KvStore<P> {
    pub fn open(path: impl Into<PathBuf>, concurrency: u32) -> Result<Self> {
        let path = Arc::new(path.into());
        fs::create_dir_all(&*path)?;

        let mut readers = BTreeMap::new();
        let index = Arc::new(SkipMap::<String, CommandPos>::new());
        
        let gen_list = sorted_gen_list(&path)?;
        let mut uncompacted = 0;
        for &gen in &gen_list {
            let mut reader = BufReaderWithPos::new(File::open(log_path(&path, gen))?)?;
            uncompacted += load(gen, &mut reader, &*index)?;
            readers.insert(gen, reader);
        }


        let current_gen = gen_list.last().unwrap_or(&0) + 1;
        let writer = new_log_file(&path, current_gen)?;
        let safe_point = Arc::new(AtomicU64::new(0));

        let reader = KvStoreReader {
            path: Arc::clone(&path),
            safe_point,
            readers: RefCell::new(BTreeMap::new()),
        };

        let writer = KvStoreWriter {
            reader: reader.clone(),
            writer,
            current_gen,
            uncompacted,
            path: Arc::clone(&path),
            index: Arc::clone(&index),
        };

        let thread_pool = P::new(concurrency)?;
        let reader_pool = Arc::new(ArrayQueue::new(concurrency as usize));
        for _ in 1 .. concurrency {
            reader_pool.push(reader.clone()).unwrap();
        }

        reader_pool.push(reader).unwrap();

        Ok(KvStore{
            path, 
            index, 
            writer: Arc::new(Mutex::new(writer)),
            thread_pool,
            reader_pool,
        })
    }
}

impl<P: ThreadPool> KvsEngine for KvStore<P> {
    fn set(&self, key: String, value: String) -> Box<dyn Future<Item = (), Error = KvsError> + Send> {
        let writer = self.writer.clone();
        let (tx, rx) = oneshot::channel();
        self.thread_pool.spawn(move || {
            let res = writer.lock().unwrap().set(key, value);
            if tx.send(res).is_err() {
                error!("Receving end is dropped")
            }
        });

        Box::new(
            rx.map_err(|e| KvsError::StringError(format!("{}", e))).flatten()
        )
    }

    fn get(&self, key: String) -> Box<dyn Future<Item = Option<String>, Error = KvsError> + Send> {
        let reader_pool = self.reader_pool.clone();
        let index = self.index.clone();
        let (tx, rx) = oneshot::channel();
        self.thread_pool.spawn(move || {
            let res = (|| {
                if let Some(cmd_pos) = index.get(&key) {
                    let reader = reader_pool.pop().unwrap();
                    let res = if let Command::Set {value, ..} = 
                        reader.read_command(*cmd_pos.value())? 
                    {
                        Ok(Some(value))
                    } else {
                        Err(KvsError::UnexpectedCommandType)
                    };
                    reader_pool.push(reader).unwrap();
                    res
                } else {
                    Ok(None)
                }
            })();
            if tx.send(res).is_err() {
                error!("Receving end is dropped")
            }
        });
        Box::new(
            rx.map_err(|e| KvsError::StringError(format!("{}", e))).flatten()
        )
    }

    fn remove(&self, key: String) -> Box<dyn Future<Item = (), Error = KvsError> + Send> {
        let writer = self.writer.clone();
        let (tx, rx) = oneshot::channel();
        self.thread_pool.spawn(move || {
            let res = writer.lock().unwrap().remove(key);
            if tx.send(res).is_err() {
                error!("Receiving end is dropped");
            }
        });
        Box::new(
            rx.map_err(|e| KvsError::StringError(format!("{}", e))).flatten(),
        )
    }
}

struct KvStoreReader {
    path: Arc<PathBuf>,
    safe_point: Arc<AtomicU64>,
    readers: RefCell<BTreeMap<u64, BufReaderWithPos<File>>>,
}

impl KvStoreReader {
    fn close_stale_handles(&self) {
        let mut readers = self.readers.borrow_mut();
        while !readers.is_empty() {
            let first_gen = *readers.keys().next().unwrap();
            if self.safe_point.load(Ordering::SeqCst) != first_gen {
                break;
            }
            readers.remove(&first_gen);
        }
    }

    fn read_and<F, R>(&self, cmd_pos: CommandPos, f: F) -> Result<R>
    where F: FnOnce(io::Take<&mut BufReaderWithPos<File>>) -> Result<R> 
    {
        self.close_stale_handles();
        let mut readers = self.readers.borrow_mut();
        if !readers.contains_key(&cmd_pos.gen) {
            let reader = BufReaderWithPos::new(File::open(log_path(&self.path, cmd_pos.gen))?)?;
            readers.insert(cmd_pos.gen, reader);
        }
        let reader = readers.get_mut(&cmd_pos.gen).unwrap();
        reader.seek(SeekFrom::Start(cmd_pos.pos))?;
        let cmd_reader = reader.take(cmd_pos.len);
        f(cmd_reader)
    }

    fn read_command(&self, cmd_pos: CommandPos) -> Result<Command> {
        self.read_and(cmd_pos, |cmd_reader| {
            Ok(serde_json::from_reader(cmd_reader)?)
        })
    }
}

impl Clone for KvStoreReader {
    fn clone(&self) -> KvStoreReader{
        KvStoreReader {
            path: Arc::clone(&self.path),
            safe_point: Arc::clone(&self.safe_point),
            readers: RefCell::new(BTreeMap::new())
        }
    }
}

struct KvStoreWriter {
    reader: KvStoreReader,
    writer: BufWriterWithPos<File>,
    current_gen: u64,
    uncompacted: u64,
    path: Arc<PathBuf>,
    index: Arc<SkipMap<String, CommandPos>>,
}

impl KvStoreWriter {
    fn set(&mut self, key: String, value: String) -> Result<()> {
        let cmd = Command::set(key, value);
        let pos = self.writer.pos;
        serde_json::to_writer(&mut self.writer, &cmd)?;
        self.writer.flush()?;
        if let Command::Set {key, .. } = cmd {
            if let Some(old_cmd) = self.index.get(&key) {
                self.uncompacted = old_cmd.value().len
            }
            self.index.insert(key, (self.current_gen, pos..self.writer.pos).into());
        }
        if self.uncompacted > COMPACTION_THRESHOLD {
            self.compact()?;
        }
        Ok(())
    }

    fn remove(&mut self, key: String) -> Result<()> {
        if self.index.contains_key(&key) {
            let cmd = Command::remove(key);
            let pos = self.writer.pos;
            serde_json::to_writer(&mut self.writer, &cmd);
            self.writer.flush()?;
            if let Command::Remove {key} = cmd {
                let old_cmd = self.index.remove(&key).expect("key not found");
                self.uncompacted += old_cmd.value().len;
                self.uncompacted += self.writer.pos - pos;
            }
            if self.uncompacted > COMPACTION_THRESHOLD {
                self.compact()?;
            }
            Ok(())
        } else {
            Err(KvsError::KeyNotFount)
        }
   }

   fn compact(&mut self) -> Result<()> {
    let compaction_gen = self.current_gen + 1;
    self.current_gen += 2;
    self.writer = new_log_file(&self.path, compaction_gen)?;   
    
    let mut compaction_writer = new_log_file(&self.path, compaction_gen)?;

    let mut new_pos = 0;
    for entry in self.index.iter() {
        let len = self.reader.read_and(*entry.value(), |mut entry_reader|{
            Ok(io::copy(&mut entry_reader, &mut compaction_writer)?)
        })?;
        self.index.insert(
            entry.key().clone(), 
            (compaction_gen, new_pos..new_pos+len).into(),
        ); 
        new_pos += len;
    }
    compaction_writer.flush()?;

    self.reader.safe_point.store(compaction_gen, Ordering::SeqCst);
    self.reader.close_stale_handles();
    let stale_gens = sorted_gen_list(&self.path)?
    .into_iter()
    .filter(|&gen| gen < compaction_gen);

    for stale_gen in stale_gens {
        let file_path = log_path(&self.path, stale_gen);
        if let Err(e) = fs::remove_file(&file_path) {
            error!("{:?} cannot be deleted: {}", file_path, e);
        }
    }
    self. uncompacted = 0;

    Ok(())
   }
}

fn new_log_file(path: &Path, gen: u64) -> Result<BufWriterWithPos<File>> {
    let path = log_path(&path, gen);
    let writer = BufWriterWithPos::new(
        OpenOptions::new().create(true).write(true).append(true).open(&path)?,
    )?;
    Ok(writer)
}

fn sorted_gen_list(path: &Path) -> Result<Vec<u64>> {
    let mut gen_list: Vec<u64> = fs::read_dir(&path)?
    .flat_map(|res| -> Result<_> {Ok(res?.path())})
    .filter(|path| path.is_file() && path.extension() == Some(".log".as_ref()))
    .flat_map(|path| {
        path.file_name()
        .and_then(OsStr::to_str)
        .map(|s| s.trim_end_matches(".log"))
        .map(str::parse::<u64>)
    })
    .flatten().collect();
    gen_list.sort_unstable();
    Ok(gen_list)
}

fn load(gen: u64, reader: &mut BufReaderWithPos<File>, index: &SkipMap<String, CommandPos>) -> Result<u64> {
    let mut pos = reader.seek(SeekFrom::Start(0))?;
    let mut stream = Deserializer::from_reader(reader).into_iter::<Command>();
    let mut uncompacted = 0;

    while let Some(cmd) = stream.next() {
        let new_pos = stream.byte_offset() as u64;
        match cmd? {
            Command::Set{key, ..} => {
                if let Some(old_cmd) = index.get(&key) {
                    uncompacted += old_cmd.value().len;
                }
                index.insert(key, (gen, pos..new_pos).into());
            }
            Command::Remove{key} => {
                if let Some(old_cmd) = index.remove(&key) {
                    uncompacted += old_cmd.value().len;
                }
                uncompacted += new_pos - pos;
            }
        }
        pos = new_pos;
    }

    Ok(uncompacted)
}


fn log_path(dir: &Path, gen: u64) -> PathBuf {
    dir.join(format!("{}.log", gen))
}


#[derive(Debug, Serialize, Deserialize)]
enum Command {
    Set {key: String, value: String},
    Remove {key: String,}
}

impl Command {
    fn set(key: String, value: String) -> Command {
        Command::Set {key, value}
    }

    fn remove(key: String) -> Command {
        Command::Remove {key}
    }
}

#[derive(Debug, Clone, Copy)]
struct CommandPos {
    gen: u64,
    pos: u64,
    len: u64
}

impl From<(u64, Range<u64>)> for CommandPos {
    fn from((gen, range): (u64, Range<u64>)) -> Self {
        CommandPos {
            gen,
            pos: range.start,
            len: range.end - range.start
        }
    }
}


struct BufReaderWithPos<R: Read + Seek> {
    reader: BufReader<R>,
    pos: u64,
}

impl <R: Read + Seek> BufReaderWithPos<R> {
    fn new(mut inner: R) -> Result<Self> {
        let pos = inner.seek(SeekFrom::Current(0))?;
        Ok(BufReaderWithPos{
            reader: BufReader::new(inner),
            pos,
        })
    }
}

impl <R: Read + Seek> Read for BufReaderWithPos<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = self.reader.read(buf)?;
        self.pos += len as u64;
        Ok(len)
    }
}

impl <R: Read + Seek> Seek for BufReaderWithPos<R> {
    fn seek(&mut self, pos: SeekFrom) ->   io::Result<u64> {
        self.pos = self.reader.seek(pos)?;
        Ok(self.pos)
    }
}
 


struct BufWriterWithPos<W: Write + Seek> {
    writer: BufWriter<W>,
    pos: u64,
}

impl<W: Write + Seek> BufWriterWithPos<W> {
    fn new(mut inner: W) -> Result<Self> {
        let pos = inner.seek(SeekFrom::Current(0))?;
        Ok(BufWriterWithPos{
            writer: BufWriter::new(inner),
            pos,
        })
    }
}
 
impl<W: Write + Seek> Write for BufWriterWithPos<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let len = self.writer.write(buf)?;
        self.pos += len as u64;
        Ok(len)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

impl<W: Write + Seek> Seek for BufWriterWithPos<W>{
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.pos = self.writer.seek(pos)?;
        Ok(self.pos)
    }
}