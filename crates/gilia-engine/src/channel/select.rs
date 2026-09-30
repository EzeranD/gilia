// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::time::{Duration, Instant};

use super::{
    Receiver, RecvError, SendError, Sender, TryRecvError, TrySendError, WaitSource,
    wait::{ThreadWaiter, WaiterKind, wait_until},
};
use crate::channel::wait::WaitError;

pub(crate) struct Receive<'a, T> {
    receiver: &'a Receiver<T>,
    result: Option<Result<T, RecvError>>,
}

pub(crate) struct Send<'a, T> {
    sender: &'a Sender<T>,
    pending: Option<T>,
    result: Option<Result<(), SendError<T>>>,
}

pub(crate) struct Ready<'a> {
    source: &'a dyn WaitSource,
    result: Option<()>,
}

pub(crate) trait Operation {
    fn source(&self) -> &dyn WaitSource;
    fn try_select(&mut self) -> bool;
}

pub(crate) enum Deadline {
    Never,
    Immediate,
    At(Instant),
}

pub(crate) fn select<const N: usize>(
    operations: &mut [&mut dyn Operation; N],
    deadline: Deadline,
) -> Result<Option<usize>, WaitError> {
    let mut waiter = None;

    loop {
        for (index, operation) in operations.iter_mut().enumerate() {
            if operation.try_select() {
                return Ok(Some(index));
            }
        }

        let deadline = match deadline {
            Deadline::Immediate => return Ok(None),
            Deadline::At(deadline) if Instant::now() >= deadline => return Ok(None),
            Deadline::At(deadline) => Some(deadline),
            Deadline::Never => None,
        };

        let waiter = waiter.get_or_insert_with(ThreadWaiter::acquire).as_waiter();
        let sources = operations.each_ref().map(|op| op.source());

        if sources.is_empty() {
            waiter.wait(deadline)?;
        } else {
            wait_until(&sources, waiter, WaiterKind::Observer, deadline)?;
        }
    }
}

impl<'a, T> Receive<'a, T> {
    pub(crate) fn new(receiver: &'a Receiver<T>) -> Self {
        Self {
            receiver,
            result: None,
        }
    }

    pub(crate) fn result(self) -> Option<Result<T, RecvError>> {
        self.result
    }
}

impl<T> Operation for Receive<'_, T> {
    fn source(&self) -> &dyn WaitSource {
        self.receiver
    }

    fn try_select(&mut self) -> bool {
        match self.receiver.try_recv() {
            Ok(value) => self.result = Some(Ok(value)),
            Err(TryRecvError::Disconnected) => {
                self.result = Some(Err(RecvError::Disconnected));
            }
            Err(TryRecvError::Empty) => return false,
        }

        true
    }
}

impl<'a, T> Send<'a, T> {
    pub(crate) fn new(sender: &'a Sender<T>, value: T) -> Self {
        Self {
            sender,
            pending: Some(value),
            result: None,
        }
    }

    pub(crate) fn result(self) -> Option<Result<(), SendError<T>>> {
        self.result
    }
}

impl<T> Operation for Send<'_, T> {
    fn source(&self) -> &dyn WaitSource {
        self.sender
    }

    fn try_select(&mut self) -> bool {
        let value = self
            .pending
            .take()
            .expect("send operation has no pending value");

        match self.sender.try_send(value) {
            Ok(()) => self.result = Some(Ok(())),
            Err(TrySendError::Disconnected { value }) => {
                self.result = Some(Err(SendError::Disconnected { value }));
            }
            Err(TrySendError::Full { value }) => {
                self.pending = Some(value);
                return false;
            }
        }

        true
    }
}

impl<'a> Ready<'a> {
    pub(crate) fn new(source: &'a dyn WaitSource) -> Self {
        Self {
            source,
            result: None,
        }
    }

    pub(crate) fn result(self) -> Option<()> {
        self.result
    }
}

impl Operation for Ready<'_> {
    fn source(&self) -> &dyn WaitSource {
        self.source
    }

    fn try_select(&mut self) -> bool {
        if !self.source.is_ready() {
            return false;
        }

        self.result = Some(());

        true
    }
}

impl Deadline {
    pub(crate) fn after(timeout: Duration) -> Self {
        match Instant::now().checked_add(timeout) {
            Some(deadline) => Self::At(deadline),
            None => Self::Never,
        }
    }
}

macro_rules! select_biased {
    (@parse [$($labels:tt)*] [$($cases:tt)*] [$($default:tt)*]
        $kind:ident($($args:expr),+ $(,)?) $(-> $binding:pat)? => $body:block, $($rest:tt)*) => {
        $crate::channel::select_biased! {
            @branch [$($labels)*] [$($cases)*] [$($default)*]
            $kind($($args),+) $(-> $binding)? => $body; $($rest)*
        }
    };
    (@parse [$($labels:tt)*] [$($cases:tt)*] [$($default:tt)*]
        $kind:ident($($args:expr),+ $(,)?) $(-> $binding:pat)? => $body:block $($rest:tt)*) => {
        $crate::channel::select_biased! {
            @branch [$($labels)*] [$($cases)*] [$($default)*]
            $kind($($args),+) $(-> $binding)? => $body; $($rest)*
        }
    };
    (@parse [$($labels:tt)*] [$($cases:tt)*] [$($default:tt)*]
        $kind:ident($($args:expr),+ $(,)?) $(-> $binding:pat)? => $body:expr, $($rest:tt)*) => {
        $crate::channel::select_biased! {
            @branch [$($labels)*] [$($cases)*] [$($default)*]
            $kind($($args),+) $(-> $binding)? => { $body }; $($rest)*
        }
    };
    (@parse [$($labels:tt)*] [$($cases:tt)*] [$($default:tt)*]
        $kind:ident($($args:expr),+ $(,)?) $(-> $binding:pat)? => $body:expr) => {
        $crate::channel::select_biased! {
            @branch [$($labels)*] [$($cases)*] [$($default)*]
            $kind($($args),+) $(-> $binding)? => { $body };
        }
    };

    (@branch [($index:tt $name:ident) $($labels:tt)*] [$($cases:tt)*] [$($default:tt)*]
        recv($source:expr) -> $binding:pat => $body:block; $($rest:tt)*) => {
        $crate::channel::select_biased! {
            @parse [$($labels)*]
            [$($cases)* ($index $name recv($source) -> $binding => $body)]
            [$($default)*] $($rest)*
        }
    };
    (@branch [($index:tt $name:ident) $($labels:tt)*] [$($cases:tt)*] [$($default:tt)*]
        send($source:expr, $value:expr) $(-> $binding:pat)? => $body:block; $($rest:tt)*) => {
        $crate::channel::select_biased! {
            @parse [$($labels)*]
            [$($cases)* ($index $name send($source, $value) $(-> $binding)? => $body)]
            [$($default)*] $($rest)*
        }
    };
    (@branch [($index:tt $name:ident) $($labels:tt)*] [$($cases:tt)*] [$($default:tt)*]
        ready($source:expr) => $body:block; $($rest:tt)*) => {
        $crate::channel::select_biased! {
            @parse [$($labels)*]
            [$($cases)* ($index $name ready($source) => $body)]
            [$($default)*] $($rest)*
        }
    };

    (@parse [$($labels:tt)*] [$($cases:tt)*] [] default() => $($rest:tt)*) => {
        $crate::channel::select_biased!(@parse [$($labels)*] [$($cases)*] [] default => $($rest)*)
    };
    (@parse [$($labels:tt)*] [$($cases:tt)*] [] default => $body:block, $($rest:tt)*) => {
        $crate::channel::select_biased!(@parse [$($labels)*] [$($cases)*] [immediate $body] $($rest)*)
    };
    (@parse [$($labels:tt)*] [$($cases:tt)*] [] default => $body:block $($rest:tt)*) => {
        $crate::channel::select_biased!(@parse [$($labels)*] [$($cases)*] [immediate $body] $($rest)*)
    };
    (@parse [$($labels:tt)*] [$($cases:tt)*] [] default => $body:expr, $($rest:tt)*) => {
        $crate::channel::select_biased!(@parse [$($labels)*] [$($cases)*] [immediate { $body }] $($rest)*)
    };
    (@parse [$($labels:tt)*] [$($cases:tt)*] [] default => $body:expr) => {
        $crate::channel::select_biased!(@parse [$($labels)*] [$($cases)*] [immediate { $body }])
    };

    (@branch [$($labels:tt)*] [$($cases:tt)*] [$first:tt $($default:tt)*]
        default($timeout:expr) => $body:block; $($rest:tt)*) => {
        compile_error!("only one default branch is allowed");
    };
    (@branch [$($labels:tt)*] [$($cases:tt)*] []
        default($timeout:expr) => $body:block; $($rest:tt)*) => {
        $crate::channel::select_biased!(@parse [$($labels)*] [$($cases)*] [timeout ($timeout) $body] $($rest)*)
    };

    (@parse [$($labels:tt)*] [$($cases:tt)+] [$($default:tt)*]) => {
        $crate::channel::select_biased!(@emit [$($cases)+] [$($default)*])
    };
    (@parse [$($labels:tt)*] [] [$first:tt $($default:tt)*]) => {
        $crate::channel::select_biased!(@emit [] [$first $($default)*])
    };

    (@emit
        [$(($index:tt $name:ident $kind:ident($($args:expr),+) $(-> $binding:pat)? => $body:block))+]
        [$($default:tt)*]) => {{
        let (selected, ($($name,)+)) = {
            $(
                let $name = $crate::channel::select_biased!(@source $kind($($args),+));
                let mut $name = $crate::channel::select_biased!(@operation $name; $kind($($args),+));
            )+
            let deadline = $crate::channel::select_biased!(@deadline [$($default)*]);
            let selected = $crate::channel::select::select(
                &mut [$(&mut $name as &mut dyn $crate::channel::select::Operation,)+],
                deadline,
            )
            .expect("thread selection failed");

            (selected, ($($name.result(),)+))
        };

        match selected {
            $(Some($index) => {
                $crate::channel::select_biased!(@bind $name $(-> $binding)?);
                $body
            },)+
            None => $crate::channel::select_biased!(@fallback [$($default)*]),
            _ => unreachable!("selection returned an invalid branch index"),
        }
    }};

    (@emit [] [$first:tt $($default:tt)*]) => {{
        let deadline = $crate::channel::select_biased!(@deadline [$first $($default)*]);
        $crate::channel::select::select(&mut [], deadline)
            .expect("thread selection failed");

        $crate::channel::select_biased!(@fallback [$first $($default)*])
    }};

    (@source recv($source:expr)) => { &($source) };
    (@source send($source:expr, $value:expr)) => { &($source) };
    (@source ready($source:expr)) => { &($source) };

    (@operation $name:ident; recv($source:expr)) => {
        $crate::channel::select::Receive::new($name)
    };
    (@operation $name:ident; send($source:expr, $value:expr)) => {
        $crate::channel::select::Send::new($name, $value)
    };
    (@operation $name:ident; ready($source:expr)) => {
        $crate::channel::select::Ready::new($name)
    };

    (@bind $name:ident -> $binding:pat) => {
        let $binding = $name.expect("selected operation has no result");
    };
    (@bind $name:ident) => {
        let _ = $name.expect("selected operation has no result");
    };

    (@deadline []) => {
        $crate::channel::select::Deadline::Never
    };
    (@deadline [immediate $body:block]) => {
        $crate::channel::select::Deadline::Immediate
    };
    (@deadline [timeout ($timeout:expr) $body:block]) => {
        $crate::channel::select::Deadline::after($timeout)
    };
    (@fallback []) => {
        unreachable!("blocking selection returned no operation")
    };
    (@fallback [immediate $body:block]) => { $body };
    (@fallback [timeout ($timeout:expr) $body:block]) => { $body };

    (@parse [$($labels:tt)*] [$($cases:tt)*] [$first:tt $($default:tt)*] default $($rest:tt)*) => {
        compile_error!("only one default branch is allowed");
    };
    (@branch [$($labels:tt)*] [$($cases:tt)*] [$($default:tt)*]
        recv($source:expr) => $body:block; $($rest:tt)*) => {
        compile_error!("recv requires a result binding: recv(receiver) -> result => body");
    };
    (@branch [] $($rest:tt)*) => {
        compile_error!("at most 32 operations are supported");
    };
    (@parse [$($labels:tt)*] [] []) => {
        compile_error!("expected at least one branch");
    };
    (@$stage:ident $($rest:tt)*) => {
        compile_error!("invalid branch: expected recv, send, ready, or default");
    };

    ($($branches:tt)*) => {
        $crate::channel::select_biased! {
            @parse [
                (0 branch0) (1 branch1) (2 branch2) (3 branch3)
                (4 branch4) (5 branch5) (6 branch6) (7 branch7)
                (8 branch8) (9 branch9) (10 branch10) (11 branch11)
                (12 branch12) (13 branch13) (14 branch14) (15 branch15)
                (16 branch16) (17 branch17) (18 branch18) (19 branch19)
                (20 branch20) (21 branch21) (22 branch22) (23 branch23)
                (24 branch24) (25 branch25) (26 branch26) (27 branch27)
                (28 branch28) (29 branch29) (30 branch30) (31 branch31)
            ] [] [] $($branches)*
        }
    };
}

pub(crate) use select_biased;
