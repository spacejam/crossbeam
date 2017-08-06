use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::atomic::Ordering::SeqCst;
use std::thread;
use std::time::{Duration, Instant};

use rand::{Rng, thread_rng};

use {Flavor, Sender, Receiver};
use actor;
use err::{TryRecvError, TrySendError};
use watch::dock::Request;
use Backoff;

fn gen_random(ceil: usize) -> usize {
    thread_local! {
        static RNG: Cell<u32> = Cell::new(1);
    }

    RNG.with(|rng| {
        let mut x = ::std::num::Wrapping(rng.get());
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        rng.set(x.0);

        let x = x.0 as usize;
        match ceil {
            0 => 0,
            1 => x % 1,
            2 => x % 2,
            3 => x % 3,
            4 => x % 4,
            n => x % n,
        }
    })
}

pub struct Select {
    machine: Machine,
    _marker: PhantomData<*mut ()>,
}

impl Select {
    #[inline]
    pub fn new() -> Self {
        Select::with_deadline(None)
    }

    #[inline]
    pub fn with_timeout(dur: Duration) -> Self {
        Select::with_deadline(Some(Instant::now() + dur))
    }

    #[inline]
    fn with_deadline(deadline: Option<Instant>) -> Self {
        Select {
            machine: Machine::Counting {
                len: 0,
                id_first: 0,
                deadline,
            },
            _marker: PhantomData,
        }
    }

    #[inline]
    pub fn timed_out(&self) -> bool {
        if let Machine::Initialized { state, .. } = self.machine {
            if let State::TimedOut = state {
                return true;
            }
        }
        false
    }

    #[inline]
    pub fn disconnected(&self) -> bool {
        if let Machine::Initialized { state, .. } = self.machine {
            if let State::Disconnected = state {
                return true;
            }
        }
        false
    }

    pub fn send<T>(&mut self, tx: &Sender<T>, mut value: T) -> Result<(), T> {
        if let Some(state) = self.machine.step(tx.id()) {
            state.send(tx, value)
        } else {
            Err(value)
        }
    }

    pub fn recv<T>(&mut self, rx: &Receiver<T>) -> Result<T, ()> {
        if let Some(state) = self.machine.step(rx.id()) {
            state.recv(rx)
        } else {
            Err(())
        }
    }
}

enum Machine {
    Counting {
        len: usize,
        id_first: usize,
        deadline: Option<Instant>,
    },
    Initialized {
        pos: usize,
        state: State,
        len: usize,
        start: usize,
        deadline: Option<Instant>,
    },
}

impl Machine {
    #[inline]
    fn step(&mut self, id: usize) -> Option<&mut State> {
        loop {
            match *self {
                Machine::Counting {
                    len,
                    id_first,
                    deadline,
                } => {
                    if id_first == id {
                        *self = Machine::Initialized {
                            pos: 0,
                            state: State::Try { closed_count: 0 },
                            len,
                            start: gen_random(len),
                            deadline,
                        };
                    } else {
                        *self = Machine::Counting {
                            len: len + 1,
                            id_first: if id_first == 0 { id } else { id_first },
                            deadline,
                        };
                        return None;
                    }
                }
                Machine::Initialized {
                    pos,
                    mut state,
                    len,
                    start,
                    deadline,
                } => {
                    if pos >= 2 * len {
                        state.transition(len, deadline);
                        *self = Machine::Initialized {
                            pos: 0,
                            state,
                            len,
                            start,
                            deadline,
                        };
                    } else {
                        *self = Machine::Initialized {
                            pos: pos + 1,
                            state,
                            len,
                            start,
                            deadline,
                        };
                        if let Machine::Initialized { ref mut state, .. } = *self {
                            if start <= pos && pos < start + len {
                                return Some(state);
                            } else {
                                return None;
                            }
                        }
                    }
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
enum State {
    Try { closed_count: usize },
    Subscribe { closed_countdown: isize },
    Unsubscribe,
    FinalTry { id: usize },
    TimedOut,
    Disconnected,
}

impl State {
    #[inline]
    fn transition(&mut self, len: usize, deadline: Option<Instant>) {
        match *self {
            State::Try { closed_count } => {
                // println!("-------- TRY -------");
                if closed_count < len {
                    actor::current().reset();
                    *self = State::Subscribe {
                        closed_countdown: closed_count as isize,
                    };
                } else {
                    *self = State::Disconnected;
                }
            }
            State::Subscribe { closed_countdown } => {
                // println!("went asleep");
                if closed_countdown < 0 {
                    actor::current().select(1);
                }
                actor::current().wait_until(deadline);
                // println!("woke up");
                *self = State::Unsubscribe;
            }
            State::Unsubscribe => {
                *self = State::FinalTry {
                    id: actor::current().selected(),
                };
            }
            State::FinalTry { .. } => {
                *self = State::Try { closed_count: 0 };

                if let Some(end) = deadline {
                    if Instant::now() >= end {
                        *self = State::TimedOut;
                    }
                }
            }
            State::TimedOut => {}
            State::Disconnected => {}
        }
    }

    fn send<T>(&mut self, tx: &Sender<T>, mut value: T) -> Result<(), T> {
        match *self {
            State::Try {
                ref mut closed_count,
            } => {
                let backoff = &mut Backoff::new();
                loop {
                    match tx.try_send_with_backoff(value, backoff) {
                        Ok(()) => return Ok(()),
                        Err(TrySendError::Full(v)) => value = v,
                        Err(TrySendError::Disconnected(v)) => {
                            value = v;
                            *closed_count += 1;
                            break;
                        }
                    }
                    if !backoff.tick() {
                        break;
                    }
                }
            }
            State::Subscribe {
                ref mut closed_countdown,
            } => {
                match tx.0.flavor {
                    Flavor::List(ref q) => {}
                    Flavor::Array(ref q) => q.senders().register(tx.id()),
                    Flavor::Zero(ref q) => q.promise_send(tx.id()),
                }
                if tx.is_disconnected() {
                    *closed_countdown -= 1;
                }
                if tx.can_send() {
                    actor::current().select(1);
                }
            }
            State::Unsubscribe => {
                match tx.0.flavor {
                    Flavor::List(ref q) => {}
                    Flavor::Array(ref q) => q.senders().unregister(tx.id()),
                    Flavor::Zero(ref q) => q.unpromise_send(tx.id()),
                }
            }
            State::FinalTry { id } => {
                // println!("final try send");
                if tx.id() == id {
                    match tx.0.flavor {
                        Flavor::Array(..) | Flavor::List(..) => {
                            match tx.try_send(value) {
                                Ok(()) => return Ok(()),
                                Err(TrySendError::Full(v)) => value = v,
                                Err(TrySendError::Disconnected(v)) => value = v,
                            }
                        }
                        Flavor::Zero(ref q) => {
                            unsafe { actor::current().put_request(value, tx.id()) };
                            return Ok(());
                        }
                    }
                }
            }
            State::TimedOut => {}
            State::Disconnected => {}
        }
        Err(value)
    }

    fn recv<T>(&mut self, rx: &Receiver<T>) -> Result<T, ()> {
        match *self {
            State::Try {
                ref mut closed_count,
            } => {
                let backoff = &mut Backoff::new();
                loop {
                    match rx.try_recv_with_backoff(backoff) {
                        Ok(v) => return Ok(v),
                        Err(TryRecvError::Empty) => {}
                        Err(TryRecvError::Disconnected) => {
                            *closed_count += 1;
                            break;
                        }
                    }
                    if !backoff.tick() {
                        break;
                    }
                }
            }
            State::Subscribe {
                ref mut closed_countdown,
            } => {
                match rx.0.flavor {
                    Flavor::List(ref q) => q.receivers().register(rx.id()),
                    Flavor::Array(ref q) => q.receivers().register(rx.id()),
                    Flavor::Zero(ref q) => q.promise_recv(rx.id()),
                }
                if rx.is_disconnected() {
                    *closed_countdown -= 1;
                }
                if rx.can_recv() {
                    actor::current().select(1);
                }
            }
            State::Unsubscribe => {
                match rx.0.flavor {
                    Flavor::List(ref q) => q.receivers().unregister(rx.id()),
                    Flavor::Array(ref q) => q.receivers().unregister(rx.id()),
                    Flavor::Zero(ref q) => q.unpromise_recv(rx.id()),
                }
            }
            State::FinalTry { id } => {
                // println!("final try recv");
                if rx.id() == id {
                    match rx.0.flavor {
                        Flavor::Array(..) | Flavor::List(..) => {
                            if let Ok(v) = rx.try_recv() {
                                return Ok(v);
                            }
                        }
                        Flavor::Zero(ref q) => {
                            let v = unsafe { actor::current().take_request(rx.id()) };
                            return Ok(v);
                        }
                    }
                }
            }
            State::TimedOut => {}
            State::Disconnected => {}
        }
        Err(())
    }
}

#[cfg(test)]
mod tests {
    use crossbeam;

    use super::*;
    use bounded;
    use unbounded;

    fn ms(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    #[test]
    fn select_recv() {
        let (tx1, rx1) = bounded::<i32>(0);
        let (tx2, rx2) = bounded::<i32>(0);

        crossbeam::scope(|s| {
            s.spawn(|| {
                // thread::sleep(ms(150));
                // tx1.send_timeout(1, ms(200));
                loop {
                    match tx1.try_send(1) {
                        Ok(()) => break,
                        Err(TrySendError::Disconnected(_)) => break,
                        Err(TrySendError::Full(_)) => continue,
                    }
                }
            });
            s.spawn(|| {
                // thread::sleep(ms(150));
                // tx2.send_timeout(2, ms(200));
                loop {
                    match tx2.try_send(2) {
                        Ok(()) => break,
                        Err(TrySendError::Disconnected(_)) => break,
                        Err(TrySendError::Full(_)) => continue,
                    }
                }
            });
            s.spawn(|| {
                thread::sleep(ms(100));
                // let mut s = Select::new();
                let mut s = Select::with_timeout(ms(100));
                loop {
                    if let Ok(x) = s.recv(&rx1) {
                        println!("{}", x);
                        break;
                    }
                    if let Ok(x) = s.recv(&rx2) {
                        println!("{}", x);
                        break;
                    }
                    if s.disconnected() {
                        println!("DISCONNECTED!");
                        break;
                    }
                    if s.timed_out() {
                        println!("TIMEOUT!");
                        break;
                    }
                }
                drop(rx1);
                drop(rx2);
            });
        });
    }

    #[test]
    fn select_send() {
        let (tx1, rx1) = bounded::<i32>(0);
        let (tx2, rx2) = bounded::<i32>(0);

        crossbeam::scope(|s| {
            s.spawn(|| {
                thread::sleep(ms(150));
                println!("got 1: {:?}", rx1.recv_timeout(ms(200)));
            });
            s.spawn(|| {
                thread::sleep(ms(150));
                println!("got 2: {:?}", rx2.recv_timeout(ms(200)));
            });
            s.spawn(|| {
                thread::sleep(ms(100));
                // let mut s = Select::new();
                let mut s = Select::with_timeout(ms(100));
                loop {
                    if let Ok(()) = s.send(&tx1, 1) {
                        println!("sent 1");
                        break;
                    }
                    if let Ok(()) = s.send(&tx2, 2) {
                        println!("sent 2");
                        break;
                    }
                    if s.disconnected() {
                        println!("DISCONNECTED!");
                        break;
                    }
                    if s.timed_out() {
                        println!("TIMEOUT!");
                        break;
                    }
                }
            });
        });
    }
}
