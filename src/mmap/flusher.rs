use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::io::{Error, ErrorKind};

use eventual::{Complete, Future};

use mmap::MmapHandle;

pub struct Flusher {
    tx: Sender<FlushOp>,
}

enum FlushOp {
    Flush(Complete<(), Error>),
    Reset(MmapHandle),
}

impl Flusher {

    pub fn with_offset(mmap: MmapHandle) -> Flusher {
        let (tx, rx) = channel::<FlushOp>();
        thread::spawn(move || flush_loop(mmap, rx));
        Flusher { tx: tx }
    }

    pub fn flush(&self) -> Future<(), Error> {
        let (complete, future) = Future::pair();
        self.tx
            .send(FlushOp::Flush(complete))
            .map(|_| future)
            .unwrap_or(Future::error(Error::new(ErrorKind::Other, "wal segment flusher stopped")))
    }
}

fn flush_loop(mut mmap: MmapHandle, rx: Receiver<FlushOp>) {
    let mut completions: Vec<Complete<(), Error>> = Vec::new();

    while let Ok(mut op) = rx.recv() {
        match op {
            FlushOp::Flush(complete) => completions.push(complete),
            FlushOp::Reset(handle) => {
                mmap = handle;
                continue;
            }
        }

        while let Ok(op) = rx.try_recv() {
            match op {
                FlushOp::Flush(complete) => completions.push(complete),
                FlushOp::Reset(handle) => {
                    mmap = handle;
                    break;
                }
            }
        }

        debug!("flushing {} entries", completions.len());

        // TODO: investigate whether doing a ranged flush is more performant.
        let result = mmap.get_mut().flush();
        match result {
            Ok(_) => {
                for complete in completions.drain(..) {
                    complete.complete(())
                }
            }
            Err(error) => {
                warn!("error while flushing mmap: {:?}", error);
                for complete in completions.drain(..) {
                    complete.fail(Error::new(error.kind(), "wal segment flush error"));
                }
                return;
            }
        }
    }
    info!("shutting down");
}

#[cfg(test)]
mod test {
}