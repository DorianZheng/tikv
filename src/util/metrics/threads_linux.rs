// Copyright 2017 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use std::fs;
use std::io::{Error, ErrorKind, Read, Result};
use std::sync::Mutex;

use libc::{self, pid_t};
use prometheus::core::{Collector, Desc};
use prometheus::{self, proto, CounterVec, IntGaugeVec, Opts};

/// monitor current process's threads.
pub fn monitor_threads<S: Into<String>>(namespace: S) -> Result<()> {
    let pid = unsafe { libc::getpid() };
    let tc = ThreadsCollector::new(pid, namespace);
    prometheus::register(Box::new(tc)).map_err(|e| to_io_err(format!("{:?}", e)))
}

/// A collector to collect threads metrics, including CPU usage
/// and threads state.
struct ThreadsCollector {
    pid: pid_t,
    descs: Vec<Desc>,
    // There are several step to collect CPU time, we need to synchronous them.
    cpu_totals: Mutex<CounterVec>,
    threads_state: IntGaugeVec,
}

impl ThreadsCollector {
    fn new<S: Into<String>>(pid: pid_t, namespace: S) -> ThreadsCollector {
        let mut descs: Vec<Desc> = vec![];
        let ns = namespace.into();
        let cpu_totals = CounterVec::new(
            Opts::new(
                "thread_cpu_seconds_total",
                "Total user and system CPU time spent in \
                 seconds by threads.",
            ).namespace(ns.clone()),
            &["name", "tid"],
        ).unwrap();
        descs.extend(cpu_totals.desc().into_iter().cloned());
        let threads_state = IntGaugeVec::new(
            Opts::new("threads_state", "Number of threads in each state.").namespace(ns),
            &["state"],
        ).unwrap();
        descs.extend(threads_state.desc().into_iter().cloned());

        ThreadsCollector {
            pid,
            descs,
            cpu_totals: Mutex::new(cpu_totals),
            threads_state,
        }
    }
}

impl Collector for ThreadsCollector {
    fn desc(&self) -> Vec<&Desc> {
        self.descs.iter().collect()
    }

    fn collect(&self) -> Vec<proto::MetricFamily> {
        // Synchronous collecting CPU time.
        let cpu_totals = self.cpu_totals.lock().unwrap();
        // Clean previous threads state.
        self.threads_state.reset();
        let tids = get_thread_ids(self.pid).unwrap();
        for tid in tids {
            if let Ok(Stat {
                name,
                state,
                utime,
                stime,
            }) = Stat::collect(self.pid, tid)
            {
                // Threads CPU time.
                let total = (utime + stime) / *CLK_TCK;
                let cpu_total = cpu_totals
                    .get_metric_with_label_values(&[&name, &format!("{}", tid)])
                    .unwrap();
                let past = cpu_total.get();
                let delta = total - past;
                if delta > 0.0 {
                    cpu_total.inc_by(delta);
                }

                // Threads states.
                let state = self
                    .threads_state
                    .get_metric_with_label_values(&[&state])
                    .unwrap();
                state.inc();
            }
        }

        let mut mfs = cpu_totals.collect();
        mfs.extend(self.threads_state.collect());
        mfs
    }
}

fn get_thread_ids(pid: pid_t) -> Result<Vec<pid_t>> {
    let mut tids = Vec::new();
    let dirs = fs::read_dir(format!("/proc/{}/task", pid))?;
    for task in dirs {
        let file_name = match task {
            Ok(t) => t.file_name(),
            Err(e) => {
                error!("fail to read task of {}, error {:?}", pid, e);
                continue;
            }
        };

        let tid = match file_name.to_str() {
            Some(tid) => match tid.parse() {
                Ok(tid) => tid,
                Err(e) => {
                    error!("fail to read task of {}, error {:?}", pid, e);
                    continue;
                }
            },
            None => {
                error!("fail to read task of {}", pid);
                continue;
            }
        };
        tids.push(tid);
    }

    Ok(tids)
}

// get thread name and the index of the last character(including ')').
fn get_thread_name(tid: pid_t, stat: &str) -> Result<(String, usize)> {
    let start = stat.find('(');
    let end = stat.rfind(')');

    if let (Some(start), Some(end)) = (start, end) {
        let raw = &stat[start + 1..end];
        let mut name = String::with_capacity(raw.len());

        // sanitize thread name.
        for c in raw.chars() {
            match c {
                // Prometheus label characters `[a-zA-Z0-9_:]`
                'a'...'z' | 'A'...'Z' | '0'...'9' | '_' | ':' => {
                    name.push(c);
                }
                '-' | ' ' => {
                    name.push('_');
                }
                _ => (),
            }
        }

        if name.is_empty() {
            name = format!("{}", tid)
        }

        return Ok((name, end));
    }

    Err(to_io_err(format!(
        "can not find thread name, stat: {}",
        stat
    )))
}

fn to_io_err(s: String) -> Error {
    Error::new(ErrorKind::Other, s)
}

struct Stat {
    name: String,
    state: String,
    utime: f64,
    stime: f64,
}

impl Stat {
    /// See more man proc.
    /// Index of utime and stime.
    const CPU_INDEX: [usize; 2] = [14 - 1, 15 - 1];
    /// Index of state.
    const PROCESS_STATE_INDEX: usize = 3 - 1;

    fn collect(pid: pid_t, tid: pid_t) -> Result<Stat> {
        let mut stat = String::new();
        fs::File::open(format!("/proc/{}/task/{}/stat", pid, tid))
            .and_then(|mut f| f.read_to_string(&mut stat))?;
        get_thread_stat_internal(tid, &stat)
    }
}

// Extracted from `Stat::collect`, for test purpose.
fn get_thread_stat_internal(tid: pid_t, stat: &str) -> Result<Stat> {
    let (name, end) = get_thread_name(tid, stat)?;
    let stats: Vec<_> = (&stat[end + 2..]).split_whitespace().collect(); // excluding ") ".
    let utime = stats
        .get(Stat::CPU_INDEX[0] - 2) // -2 because pid and comm is truncated.
        .unwrap_or(&"0")
        .parse()
        .map_err(|e| to_io_err(format!("{:?}: {}", e, stat)))?;
    let stime = stats
        .get(Stat::CPU_INDEX[1] - 2)
        .unwrap_or(&"0")
        .parse()
        .map_err(|e| to_io_err(format!("{:?}: {}", e, stat)))?;
    let state = stats
        .get(Stat::PROCESS_STATE_INDEX - 2)
        .unwrap_or(&"unknown")
        .to_string();
    Ok(Stat {
        name,
        state,
        utime,
        stime,
    })
}

lazy_static! {
    // getconf CLK_TCK
    static ref CLK_TCK: f64 = {
        unsafe {
            libc::sysconf(libc::_SC_CLK_TCK) as f64
        }
    };
}

#[cfg(test)]
mod tests {
    use std::sync;
    use std::thread;

    use libc;

    use super::*;

    #[test]
    fn test_thread_stat() {
        let name = "theadnametest66";
        let (tx, rx) = sync::mpsc::channel();
        let (tx1, rx1) = sync::mpsc::channel();
        let h = thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                tx1.send(()).unwrap();
                rx.recv().unwrap();
            })
            .unwrap();
        rx1.recv().unwrap();

        let pid = unsafe { libc::getpid() };
        let tids = get_thread_ids(pid).unwrap();
        assert!(tids.len() >= 2);

        tids.iter()
            .find(|t| {
                Stat::collect(pid, **t)
                    .map(|stat| stat.name == name)
                    .unwrap_or(false)
            })
            .unwrap();

        tx.send(()).unwrap();
        h.join().unwrap();
    }

    #[test]
    fn test_get_thread_name() {
        let cases = [
            ("(ok123)", "ok123", 6),
            ("(a-b)", "a_b", 4),
            ("(Az_1:)", "Az_1:", 6),
            ("(@123)", "123", 5),
            ("1 (ab) 1", "ab", 5),
            ("1 (a b) 1", "a_b", 6),
            ("1 ((a b )) 1", "a_b_", 9),
        ];
        for &(i, o, idx) in &cases {
            let (name, end) = get_thread_name(1, i).unwrap();
            assert_eq!(name, o);
            assert_eq!(end, idx);
        }

        assert_eq!(get_thread_name(1, "(@#)").unwrap().0, "1");
        assert!(get_thread_name(1, "invalid_stat").is_err());
    }

    #[test]
    fn test_get_thread_stat() {
        let sample = "2810 (test thd) S 2550 2621 2621 0 -1 4210688 2632 0 52 0 839 138 0 \
                      0 20 0 4 0 13862 709652480 3647 18446744073709551615 4194304 4319028 \
                      140732554845776 140732554845392 140439688777693 0 0 4096 16384 0 0 0 17 3 \
                      0 0 245 0 0 6417696 6421000 8478720 140732554851684 140732554851747 \
                      140732554851747 140732554854339 0";

        let Stat {
            name,
            state,
            utime,
            stime,
        } = get_thread_stat_internal(2810, sample).unwrap();
        assert_eq!(name, "test_thd");
        assert_eq!(state, "S");
        assert_eq!(utime as i64, 839);
        assert_eq!(stime as i64, 138);
    }

    #[test]
    fn test_smoke() {
        monitor_threads("smoke").unwrap();
    }
}
