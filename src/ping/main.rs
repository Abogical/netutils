extern crate syscall;
extern crate event;

use event::EventQueue;
use std::cell::RefCell;
use std::env::args;
use std::fs::File;
use std::io::{Read, Write, Result, Error};
use std::mem;
use std::net::Ipv4Addr;
use std::ops::{DerefMut, Deref};
use std::os::unix::io::FromRawFd;
use std::process;
use std::rc::Rc;
use std::slice;
use std::str::FromStr;
use syscall::data::TimeSpec;

static PING_PERIOD: TimeSpec = TimeSpec {
    tv_sec: 1,
    tv_nsec: 0,
};

static PING_TIMEOUT_S: i64 = 5;

const PING_PACKETS_TO_SEND: usize = 4;

#[repr(C)]
struct EchoPayload {
    seq: u16,
    timestamp: TimeSpec,
    payload: [u8; 40],
}

impl Deref for EchoPayload {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        unsafe {
            slice::from_raw_parts(self as *const EchoPayload as *const u8,
                                  mem::size_of::<EchoPayload>()) as &[u8]
        }
    }
}

impl DerefMut for EchoPayload {
    fn deref_mut(&mut self) -> &mut [u8] {
        unsafe {
            slice::from_raw_parts_mut(self as *mut EchoPayload as *mut u8,
                                      mem::size_of::<EchoPayload>()) as &mut [u8]
        }
    }
}

struct Ping {
    remote_host: Ipv4Addr,
    time_file: File,
    echo_file: File,
    seq: usize,
    recieved: usize,
    //TODO: replace with BTreeMap once TimeSpec implements Ord
    waiting_for: Vec<(TimeSpec, usize)>,
}

fn time_diff_ms(from: &TimeSpec, to: &TimeSpec) -> f32 {
    return (((to.tv_sec - from.tv_sec) * 1_000_000i64 +
             ((to.tv_nsec - from.tv_nsec) as i64) / 1_000i64)) as f32 / 1_000.0f32;
}

fn add_time(a: &TimeSpec, b: &TimeSpec) -> TimeSpec {
    let mut secs = a.tv_sec + b.tv_sec;

    let mut nsecs = a.tv_nsec + b.tv_nsec;
    while nsecs >= 1000000000 {
        nsecs -= 1000000000;
        secs += 1;
    }

    TimeSpec {
        tv_sec: secs,
        tv_nsec: nsecs,
    }
}

impl Ping {
    pub fn new(remote_host: Ipv4Addr, echo_file: File, time_file: File) -> Ping {
        Ping {
            remote_host,
            echo_file,
            time_file,
            seq: 0,
            recieved: 0,
            waiting_for: vec![],
        }
    }

    pub fn on_echo_event(&mut self) -> Result<Option<()>> {
        let mut payload = EchoPayload {
            seq: 0,
            timestamp: TimeSpec::default(),
            payload: [0; 40],
        };
        let readed = self.echo_file.read(&mut payload)?;
        if readed == 0 {
            return Ok(None);
        }
        if readed < mem::size_of::<EchoPayload>() {
            return Err(Error::from_raw_os_error(syscall::EINVAL));
        }
        let mut time = TimeSpec::default();
        syscall::clock_gettime(syscall::CLOCK_MONOTONIC, &mut time)
            .map_err(|err| Error::from_raw_os_error(err.errno))?;
        let remote_host = self.remote_host;
        let mut recieved = 0;
        self.waiting_for
            .retain(|&(_ts, seq)| if seq as u16 == payload.seq {
                        recieved += 1;
                        println!("From {} icmp_seq={} time={}ms",
                                 remote_host,
                                 seq,
                                 time_diff_ms(&payload.timestamp, &time));
                        false
                    } else {
                        true
                    });
        self.recieved += recieved;
        self.is_finished()
    }

    pub fn on_time_event(&mut self) -> Result<Option<()>> {
        let mut time = TimeSpec::default();
        if self.time_file.read(&mut time)? < mem::size_of::<TimeSpec>() {
            return Err(Error::from_raw_os_error(syscall::EINVAL));
        }
        self.send_ping(&time)?;
        self.check_timeouts(&time)?;
        let waiting_till = add_time(&time, &PING_PERIOD);
        if self.time_file.write(&waiting_till)? < mem::size_of::<TimeSpec>() {
            return Err(Error::from_raw_os_error(syscall::EINVAL));
        }
        self.is_finished()
    }

    fn send_ping(&mut self, time: &TimeSpec) -> Result<Option<()>> {
        if self.seq >= PING_PACKETS_TO_SEND {
            return Ok(None);
        }
        let payload = EchoPayload {
            seq: self.seq as u16,
            timestamp: *time,
            payload: [1; 40],
        };
        let _ = self.echo_file.write(&payload)?;
        let mut timeout_time = *time;
        timeout_time.tv_sec += PING_TIMEOUT_S;
        self.waiting_for.push((timeout_time, self.seq));
        self.seq += 1;
        Ok(None)
    }

    fn check_timeouts(&mut self, time: &TimeSpec) -> Result<Option<()>> {
        let remote_host = self.remote_host;
        self.waiting_for
            .retain(|&(ts, seq)| if ts.tv_sec <= time.tv_sec {
                        println!("From {} icmp_seq={} timeout", remote_host, seq);
                        false
                    } else {
                        true
                    });
        Ok(None)
    }

    fn is_finished(&self) -> Result<Option<()>> {
        if self.seq == PING_PACKETS_TO_SEND && self.waiting_for.is_empty() {
            Ok(Some(()))
        } else {
            Ok(None)
        }
    }

    fn get_transmitted(&self) -> usize {
        self.seq
    }

    fn get_recieved(&self) -> usize {
        self.recieved
    }
}

fn main() {
    let remote_host = args().nth(1).expect("Need an address to ping");
    let remote_host = Ipv4Addr::from_str(&remote_host).expect("Can't parse the address");
    let icmp_path = format!("icmp:echo/{}", remote_host);

    let echo_fd = match syscall::open(&icmp_path, syscall::O_RDWR | syscall::O_NONBLOCK) {
        Ok(fd) => fd,
        Err(err) => {
            println!("Can't open path {} {}", icmp_path, err);
            process::exit(1);
        }
    };

    let time_path = format!("time:{}", syscall::CLOCK_MONOTONIC);

    let time_fd = match syscall::open(&time_path, syscall::O_RDWR) {
        Ok(fd) => fd,
        Err(err) => {
            println!("Can't open path {} {}", icmp_path, err);
            process::exit(1);
        }
    };

    let ping = Rc::new(RefCell::new(Ping::new(remote_host,
                                              unsafe { File::from_raw_fd(echo_fd) },
                                              unsafe { File::from_raw_fd(time_fd) })));

    let mut event_queue = EventQueue::<()>::new().expect("Can't create event queue");

    let ping_ = ping.clone();

    event_queue
        .add(echo_fd, move |_| ping_.borrow_mut().on_echo_event())
        .expect("Can't wait for echo events");

    let ping_ = ping.clone();
    event_queue
        .add(time_fd, move |_| ping_.borrow_mut().on_time_event())
        .expect("Can't wait for time events");

    event_queue
        .trigger_all(0)
        .expect("Can't trigger all ping event");

    event_queue.run().expect("Can't run even queue");

    let transmited = ping.borrow().get_transmitted();
    let recieved = ping.borrow().get_recieved();
    println!("--- {} ping statistics ---", remote_host);
    println!("{} packets transmitted, {} recieved, {}% packet loss",
             transmited,
             recieved,
             100 * (transmited - recieved) / transmited);

    process::exit(0);
}
