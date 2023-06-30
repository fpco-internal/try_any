use core::fmt::{Display, Formatter};
use core::mem;
use core::pin::Pin;
use futures::task::{Context, Poll};
use futures::*;
use std::sync::atomic::*;
use std::sync::*;

#[derive(Debug)]
pub enum TryAnyErrors {
    AllErrs(Vec<anyhow::Error>),
    ErrsUntilOk(Vec<anyhow::Error>),
}
impl Display for TryAnyErrors {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for TryAnyErrors {}

pub struct VecWithCurr<A> {
    vec: Vec<Arc<A>>,
    curr: AtomicUsize,
}
impl<A> VecWithCurr<A> {
    pub fn new<I>(vec: I) -> Self
    where
        I: IntoIterator<Item = A>,
    {
        VecWithCurr {
            vec: vec.into_iter().map(Arc::new).collect(),
            curr: 0.into(),
        }
    }

    pub fn try_any_from_curr<F, R>(&self, f: F) -> Result<R, TryAnyErrors>
    where
        F: Fn(Arc<A>) -> Result<R, anyhow::Error>,
    {
        let curr_idx = self.curr.load(Ordering::SeqCst);
        let iter = self.vec.iter().enumerate();
        let iter = iter.clone().skip(curr_idx).chain(iter.take(curr_idx));
        let mut results = vec![];

        for (i, item) in iter {
            match f(item.clone()) {
                Ok(x) => {
                    self.curr.store(i, Ordering::SeqCst);
                    return Ok(x);
                }
                Err(e) => results.push(e),
            }
        }

        Err(TryAnyErrors::AllErrs(results))
    }

    pub async fn try_any_from_curr_async<F, FR, R>(&self, f: F) -> Result<R, TryAnyErrors>
    where
        F: Fn(Arc<A>) -> FR,
        FR: futures::future::Future<Output = Result<R, anyhow::Error>>,
    {
        let curr_idx = self.curr.load(Ordering::SeqCst);
        let iter = self.vec.iter().enumerate();
        let iter = iter.clone().skip(curr_idx).chain(iter.take(curr_idx));
        let mut results = vec![];

        for (i, item) in iter {
            match f(item.clone()).await {
                Ok(x) => {
                    self.curr.store(i, Ordering::SeqCst);
                    return Ok(x);
                }
                Err(e) => results.push(e),
            }
        }

        Err(TryAnyErrors::AllErrs(results))
    }

    pub async fn try_any_async_parallel<F, FR, R>(&self, f: F) -> Result<R, TryAnyErrors>
    where
        F: Fn(Arc<A>) -> FR,
        FR: futures::future::Future<Output = Result<R, anyhow::Error>> + Unpin,
    {
        SelectOkOrAllErrs {
            inner: self.vec.iter().map(|x| f(x.clone())).collect(),
            errors: Arc::new(vec![]),
        }
        .await
        .map(|(r, _frs)| r)
        .map_err(|a_v_e| {
            TryAnyErrors::AllErrs(
                Arc::into_inner(a_v_e)
                    .expect("try_any_async_parallel: more than one strong ref to errors"),
            )
        })
    }
}

struct SelectOkOrAllErrs<Fut: TryFuture> {
    inner: Vec<Fut>,
    errors: Arc<Vec<Fut::Error>>,
}
impl<Fut: TryFuture + Unpin> Unpin for SelectOkOrAllErrs<Fut> {}
impl<Fut: TryFuture + Unpin> Future for SelectOkOrAllErrs<Fut> {
    type Output = Result<(Fut::Ok, Vec<Fut>), Arc<Vec<Fut::Error>>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            let item =
                self.inner
                    .iter_mut()
                    .enumerate()
                    .find_map(|(i, f)| match f.try_poll_unpin(cx) {
                        Poll::Pending => None,
                        Poll::Ready(e) => Some((i, e)),
                    });
            match item {
                Some((idx, res)) => {
                    drop(self.inner.remove(idx));
                    match res {
                        Ok(e) => {
                            let rest = mem::take(&mut self.inner);
                            return Poll::Ready(Ok((e, rest)));
                        }
                        Err(e) => {
                            let errors =
                                Arc::get_mut(&mut self.errors).expect("select ok or all errs");
                            errors.push(e);
                            if self.inner.is_empty() {
                                return Poll::Ready(Err(self.errors.clone()));
                            }
                        }
                    }
                }
                None => {
                    return Poll::Pending;
                }
            }
        }
    }
}
