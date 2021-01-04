mod cancellation;
pub(crate) mod completion;

use std::io;
use std::mem;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_core::ready;
use iou::SQE;

use crate::drive::{self, Drive};

pub use cancellation::{Cancel, CancelNarrow, Cancellation, RawFdCancellation};
pub(crate) use completion::Completion;

use State::*;

/// A low-level primitive for building an IO object on io-uring.
///
/// Ring is a state machine similar to `Submission`, but it is designed to cycle through multiple
/// IO events submitted to io-uring, rather than representing a single submission. Because of this,
/// it is more low level, but it is suitable for building an IO object like a `File` on top of
/// io-uring.
///
/// Users writing code on top of `Ring` are responsible for making sure that it is correct. For
/// example, when calling `poll`, users must ensure that they are in the proper state to submit
/// whatever type of IO they would be attempting to submit. Additionally, users should note that
/// `Ring` does not implement `Drop`. In order to cancel any ongoing IO, users are responsible for
/// implementing drop to call cancel properly.
pub struct Ring<D: Drive> {
    state: State,
    driver: D,
}

enum State {
    Inert,
    Prepared(Completion),
    Submitted(Completion),
    Cancelled(u64),
    Lost,
}

impl<D: Default + Drive> Default for Ring<D> {
    fn default() -> Ring<D> {
        Ring::new(D::default())
    }
}

impl<D: Drive + Clone> Clone for Ring<D> {
    fn clone(&self) -> Ring<D> {
        Ring::new(self.driver.clone())
    }
}

impl<D: Drive> Ring<D> {
    /// Construct a new Ring on top of a driver.
    #[inline(always)]
    pub fn new(driver: D) -> Ring<D> {
        Ring {
            state: Inert,
            driver,
        }
    }

    /// Access the driver this ring is using.
    pub fn driver(&self) -> &D {
        &self.driver
    }

    /// Poll the ring state machine.
    ///
    /// This accepts a callback, `prepare`, which prepares an event to be submitted to io-uring.
    /// This callback will only be called once during an iteration of ring's state machine: once an
    /// event has been prepared, until it is completed or cancelled, a single ring instance will
    /// not prepare any additional events.
    #[inline]
    pub fn poll(
        mut self: Pin<&mut Self>,
        ctx: &mut Context<'_>,
        prepare: impl FnOnce(&mut SQE),
    ) -> Poll<io::Result<u32>> {
        match self.state {
            Inert | Cancelled(_) => {
                ready!(self.as_mut().poll_prepare(ctx, prepare));
                ready!(self.as_mut().poll_submit(ctx));
                Poll::Pending
            }
            Prepared(_) => match self.as_mut().poll_complete(ctx) {
                ready @ Poll::Ready(..) => ready,
                Poll::Pending => {
                    ready!(self.poll_submit(ctx));
                    Poll::Pending
                }
            },
            Submitted(_) => self.poll_complete(ctx),
            Lost => panic!("Ring in a bad state; driver is faulty"),
        }
    }

    #[inline(always)]
    fn poll_prepare(
        self: Pin<&mut Self>,
        ctx: &mut Context<'_>,
        prepare: impl FnOnce(&mut SQE),
    ) -> Poll<()> {
        let (driver, state) = self.split();
        let completion = match *state {
            Cancelled(prev) => ready!(driver.poll_prepare(ctx, 2, |mut sqs, ctx| {
                *state = Lost;
                unsafe {
                    sqs.hard_linked().next().unwrap().prep_cancel(prev, 0);
                }
                let mut sqe = sqs.single().unwrap();
                prepare(&mut sqe);
                drive::Completion::new(sqe, sqs, ctx)
            })),
            Inert => ready!(driver.poll_prepare(ctx, 1, |mut sqs, ctx| {
                *state = Lost;
                let mut sqe = sqs.single().unwrap();
                prepare(&mut sqe);
                drive::Completion::new(sqe, sqs, ctx)
            })),
            _ => unreachable!(),
        };
        *state = Prepared(completion.real);
        Poll::Ready(())
    }

    #[inline(always)]
    fn poll_submit(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<()> {
        let (driver, state) = self.split();
        // TODO figure out how to handle this result
        let _ = ready!(driver.poll_submit(ctx));
        if let Prepared(completion) | Submitted(completion) = mem::replace(state, Lost) {
            *state = Submitted(completion);
            Poll::Ready(())
        } else {
            unreachable!()
        }
    }

    #[inline(always)]
    fn poll_complete(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<io::Result<u32>> {
        let (_, state) = self.split();
        match mem::replace(state, Lost) {
            Prepared(completion) => match completion.check(ctx.waker()) {
                Ok(result) => {
                    *state = Inert;
                    Poll::Ready(result)
                }
                Err(completion) => {
                    *state = Prepared(completion);
                    Poll::Pending
                }
            },
            Submitted(completion) => match completion.check(ctx.waker()) {
                Ok(result) => {
                    *state = Inert;
                    Poll::Ready(result)
                }
                Err(completion) => {
                    *state = Submitted(completion);
                    Poll::Pending
                }
            },
            _ => unreachable!(),
        }
    }

    /// Cancel any ongoing IO with this cancellation.
    ///
    /// Users are responsible for ensuring that the cancellation passed would be appropriate to
    /// clean up the resources of the running event.
    #[inline]
    pub fn cancel(&mut self, cancellation: Cancellation) {
        self.state.cancel(cancellation);
    }

    /// Cancel any ongoing IO, but from a pinned reference.
    ///
    /// This has the same behavior of as Ring::cancel.
    pub fn cancel_pinned(self: Pin<&mut Self>, cancellation: Cancellation) {
        self.split().1.cancel(cancellation);
    }

    fn split(self: Pin<&mut Self>) -> (Pin<&mut D>, &mut State) {
        unsafe {
            let this = Pin::get_unchecked_mut(self);
            (Pin::new_unchecked(&mut this.driver), &mut this.state)
        }
    }
}

impl State {
    fn cancel(&mut self, cancellation: Cancellation) {
        match mem::replace(self, Lost) {
            Prepared(completion) | Submitted(completion) => {
                *self = Cancelled(completion.addr());
                completion.cancel(cancellation);
            }
            state => {
                *self = state;
            }
        }
    }
}
