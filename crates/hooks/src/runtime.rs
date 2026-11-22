use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use bytes::Bytes;
use hubdouble_model::{App, Clock, Delivery, DeliveryRequest, DeliveryResponse};
use hubdouble_store::{
    Apps, Committed, Counters, Deliveries, OutboxEntry, Sequence, Snapshot, Store,
};
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::{Semaphore, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::delivery::{Outcome, send, signature_1, signature_256};
use crate::emit::REDELIVERY_KEY;
use crate::{HooksError, Options, Stats};

pub(crate) struct Job {
    id: i64,
    epoch: u64,
    app_id: i64,
    installation_id: Option<i64>,
    repo_id: Option<i64>,
    event: String,
    action: Option<String>,
    guid: String,
    redelivery: bool,
    url: String,
    headers: BTreeMap<String, String>,
    body: Bytes,
    payload: Value,
}

#[derive(Default)]
struct AppLog {
    running: BTreeSet<i64>,
    ready: BTreeMap<i64, (u64, Delivery)>,
    logged: Option<VecDeque<i64>>,
}

#[derive(Default)]
struct State {
    queue: VecDeque<Job>,
    apps: HashMap<i64, AppLog>,
    unlogged: BTreeSet<i64>,
    in_flight: BTreeMap<u64, usize>,
    max_queued: usize,
}

pub(crate) struct Shared {
    store: Store,
    clock: Arc<dyn Clock>,
    options: Options,
    state: Mutex<State>,
    items: Semaphore,
    space: Semaphore,
    changed: watch::Sender<u64>,
    drains: mpsc::UnboundedSender<oneshot::Sender<i64>>,
    last_id: AtomicI64,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl Shared {
    pub(crate) fn start(
        store: Store,
        clock: Arc<dyn Clock>,
        options: Options,
    ) -> Result<Arc<Shared>, HooksError> {
        let rt = tokio::runtime::Handle::try_current().map_err(|_| HooksError::NoRuntime)?;
        let (drains, drain_rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            store,
            clock,
            state: Mutex::new(State::default()),
            items: Semaphore::new(0),
            space: Semaphore::new(options.queue_capacity),
            changed: watch::Sender::new(0),
            drains,
            last_id: AtomicI64::new(0),
            tasks: Mutex::new(Vec::new()),
            options,
        });
        let mut tasks = vec![rt.spawn(publish(shared.clone(), drain_rx))];
        for _ in 0..shared.options.workers.max(1) {
            tasks.push(rt.spawn(work(shared.clone())));
        }
        *shared.tasks.lock() = tasks;
        Ok(shared)
    }

    fn notify(&self) {
        self.changed.send_modify(|v| *v = v.wrapping_add(1));
    }

    async fn wait_until(&self, done: impl Fn(&State) -> bool) -> Result<(), HooksError> {
        let mut changed = self.changed.subscribe();
        loop {
            changed.borrow_and_update();
            if done(&self.state.lock()) {
                return Ok(());
            }
            changed.changed().await.map_err(|_| HooksError::Stopped)?;
        }
    }

    pub(crate) async fn drain(&self) -> Result<(), HooksError> {
        let (tx, rx) = oneshot::channel();
        self.drains.send(tx).map_err(|_| HooksError::Stopped)?;
        let last = rx.await.map_err(|_| HooksError::Stopped)?;
        self.wait_until(|st| st.unlogged.first().is_none_or(|id| *id > last))
            .await
    }

    pub(crate) async fn reset(&self, snapshot: &Snapshot) -> Result<(), HooksError> {
        self.store.restore(snapshot);
        let epoch = self.store.epoch();
        {
            let mut st = self.state.lock();
            let dropped: Vec<Job> = st.queue.drain(..).collect();
            self.space.add_permits(dropped.len());
            for job in dropped {
                self.finish(&mut st, job.app_id, job.id, None);
            }
            let stale: Vec<(i64, i64)> = st
                .apps
                .iter()
                .flat_map(|(app, log)| {
                    log.ready
                        .iter()
                        .filter(|(_, (e, _))| *e != epoch)
                        .map(|(id, _)| (*app, *id))
                })
                .collect();
            for (app, id) in stale {
                if let Some(log) = st.apps.get_mut(&app) {
                    log.ready.remove(&id);
                }
                st.unlogged.remove(&id);
            }
        }
        self.notify();
        self.wait_until(|st| st.in_flight.range(..epoch).next().is_none())
            .await
    }

    #[cfg(test)]
    pub(crate) async fn wait_stats(&self, f: impl Fn(&Stats) -> bool) {
        let _ = self.wait_until(|st| f(&Self::stats_of(st))).await;
    }

    pub(crate) fn stats(&self) -> Stats {
        Self::stats_of(&self.state.lock())
    }

    fn stats_of(st: &State) -> Stats {
        Stats {
            queued: st.queue.len(),
            max_queued: st.max_queued,
            in_flight: st.in_flight.values().sum(),
            unlogged: st.unlogged.len(),
        }
    }

    pub(crate) async fn shutdown(&self) {
        self.items.close();
        self.space.close();
        let tasks = std::mem::take(&mut *self.tasks.lock());
        let mut tasks = tasks.into_iter();
        if let Some(publisher) = tasks.next() {
            publisher.abort();
            let _ = publisher.await;
        }
        for worker in tasks {
            let _ = worker.await;
        }
        self.notify();
    }

    fn assign(&self, batch: Vec<Committed>) -> Vec<(i64, Arc<App>, Committed)> {
        self.store
            .update(|tx| {
                let epoch = self.store.epoch();
                let mut out = Vec::with_capacity(batch.len());
                for c in batch {
                    if c.epoch != epoch {
                        continue;
                    }
                    let Some(app) = tx.table::<Apps>().get(c.entry.app_id).cloned() else {
                        continue;
                    };
                    if !app.webhook.as_ref().is_some_and(|w| w.active) {
                        continue;
                    }
                    let id = tx.table_mut::<Counters>().next(Sequence::Delivery);
                    out.push((id, app, c));
                }
                Ok(out)
            })
            .unwrap_or_default()
    }

    fn job(id: i64, epoch: u64, app: &App, entry: OutboxEntry) -> Option<Job> {
        let hook = app.webhook.as_ref()?;
        let mut payload = entry.payload;
        let marker = payload
            .as_object_mut()
            .and_then(|o| o.remove(REDELIVERY_KEY));
        let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        let (guid, redelivery, event, action) = match marker {
            Some(m) => (
                text(&m, "guid")?,
                true,
                text(&m, "event")?,
                text(&m, "action"),
            ),
            None => (
                uuid::Uuid::new_v4().to_string(),
                false,
                entry.event.to_owned(),
                entry.action.map(str::to_owned),
            ),
        };
        let body = Bytes::from(serde_json::to_vec(&payload).ok()?);
        let app_id = app.id.to_string();
        let mut headers = BTreeMap::from([
            ("Accept".to_owned(), "*/*".to_owned()),
            ("Content-Type".to_owned(), "application/json".to_owned()),
            (
                "User-Agent".to_owned(),
                "GitHub-Hookshot/hubdouble".to_owned(),
            ),
            ("X-GitHub-Delivery".to_owned(), guid.clone()),
            ("X-GitHub-Event".to_owned(), event.clone()),
            ("X-GitHub-Hook-ID".to_owned(), app_id.clone()),
            ("X-GitHub-Hook-Installation-Target-ID".to_owned(), app_id),
            (
                "X-GitHub-Hook-Installation-Target-Type".to_owned(),
                "integration".to_owned(),
            ),
        ]);
        if let Some(secret) = hook.secret.as_deref().filter(|s| !s.is_empty()) {
            headers.insert(
                "X-Hub-Signature".to_owned(),
                signature_1(secret.as_bytes(), &body),
            );
            headers.insert(
                "X-Hub-Signature-256".to_owned(),
                signature_256(secret.as_bytes(), &body),
            );
        }
        Some(Job {
            id,
            epoch,
            app_id: app.id,
            installation_id: entry.installation_id,
            repo_id: entry.repo_id,
            event,
            action,
            guid,
            redelivery,
            url: hook.url.clone(),
            headers,
            body,
            payload,
        })
    }

    async fn enqueue(&self, batch: Vec<Committed>) -> Result<(), HooksError> {
        let assigned = self.assign(batch);
        let Some((last, _, _)) = assigned.last() else {
            return Ok(());
        };
        self.last_id.store(*last, Ordering::SeqCst);
        let jobs: Vec<Result<Job, (i64, i64)>> = assigned
            .into_iter()
            .map(|(id, app, c)| {
                let app_id = app.id;
                Shared::job(id, c.epoch, &app, c.entry).ok_or((app_id, id))
            })
            .collect();
        {
            let mut st = self.state.lock();
            for job in &jobs {
                let (app, id) = match job {
                    Ok(j) => (j.app_id, j.id),
                    Err(pair) => *pair,
                };
                st.unlogged.insert(id);
                st.apps.entry(app).or_default().running.insert(id);
            }
        }
        for job in jobs {
            let job = match job {
                Ok(job) => job,
                Err((app, id)) => {
                    self.finish(&mut self.state.lock(), app, id, None);
                    continue;
                }
            };
            self.space
                .acquire()
                .await
                .map_err(|_| HooksError::Stopped)?
                .forget();
            let mut st = self.state.lock();
            if job.epoch == self.store.epoch() {
                st.queue.push_back(job);
                st.max_queued = st.max_queued.max(st.queue.len());
                drop(st);
                self.items.add_permits(1);
            } else {
                self.space.add_permits(1);
                self.finish(&mut st, job.app_id, job.id, None);
            }
        }
        self.notify();
        Ok(())
    }

    fn take_job(&self) -> Option<Job> {
        let mut st = self.state.lock();
        let job = st.queue.pop_front()?;
        self.space.add_permits(1);
        if job.epoch != self.store.epoch() {
            self.finish(&mut st, job.app_id, job.id, None);
            return None;
        }
        *st.in_flight.entry(job.epoch).or_default() += 1;
        Some(job)
    }

    async fn attempt(&self, job: Job) {
        let (app, id, epoch) = (job.app_id, job.id, job.epoch);
        let delivered_at = self.clock.now();
        let started = Instant::now();
        let outcome = tokio::time::timeout(
            self.options.timeout,
            send(&*self.options.connector, &job.url, &job.headers, job.body),
        )
        .await
        .unwrap_or_else(|_| Outcome::failed("Timed out"));
        let delivery = Delivery {
            id: job.id,
            guid: job.guid,
            app_id: job.app_id,
            hook_id: job.app_id,
            installation_id: job.installation_id,
            repo_id: job.repo_id,
            event: job.event,
            action: job.action,
            url: job.url,
            delivered_at,
            redelivery: job.redelivery,
            duration: started.elapsed().as_secs_f64(),
            status: outcome.status,
            status_code: outcome.status_code,
            request: DeliveryRequest {
                headers: job.headers,
                payload: job.payload,
            },
            response: DeliveryResponse {
                headers: outcome.headers,
                payload: outcome.body,
            },
        };
        let mut st = self.state.lock();
        self.finish(&mut st, app, id, Some((epoch, delivery)));
        if let Some(n) = st.in_flight.get_mut(&epoch) {
            *n -= 1;
            if *n == 0 {
                st.in_flight.remove(&epoch);
            }
        }
    }

    fn finish(&self, st: &mut State, app: i64, id: i64, delivery: Option<(u64, Delivery)>) {
        let log = st.apps.entry(app).or_default();
        log.running.remove(&id);
        match delivery {
            Some(d) => {
                log.ready.insert(id, d);
            }
            None => {
                st.unlogged.remove(&id);
            }
        }
        self.flush(st, app);
    }

    fn flush(&self, st: &mut State, app: i64) {
        let State { apps, unlogged, .. } = st;
        let Some(log) = apps.get_mut(&app) else {
            return;
        };
        let floor = log.running.first().copied().unwrap_or(i64::MAX);
        let rest = log.ready.split_off(&floor);
        let ready = std::mem::replace(&mut log.ready, rest);
        if ready.is_empty() {
            return;
        }
        let capacity = self.options.log_capacity.max(1);
        let logged = &mut log.logged;
        let _ = self.store.update(|tx| {
            let epoch = self.store.epoch();
            let logged = logged.get_or_insert_with(|| {
                tx.table::<Deliveries>()
                    .children(app)
                    .map(|d| d.id)
                    .collect()
            });
            let mut table = tx.table_mut::<Deliveries>();
            for (e, d) in ready.values() {
                if *e != epoch {
                    continue;
                }
                if table.insert(d.clone()).is_ok() {
                    logged.push_back(d.id);
                }
                while logged.len() > capacity {
                    if let Some(old) = logged.pop_front() {
                        table.remove(old);
                    }
                }
            }
            Ok(())
        });
        for id in ready.keys() {
            unlogged.remove(id);
        }
    }
}

async fn publish(shared: Arc<Shared>, mut drains: mpsc::UnboundedReceiver<oneshot::Sender<i64>>) {
    loop {
        let mut waiting = Vec::new();
        tokio::select! {
            () = shared.store.outbox_ready() => {}
            d = drains.recv() => match d {
                Some(d) => waiting.push(d),
                None => return,
            },
        }
        while let Ok(d) = drains.try_recv() {
            waiting.push(d);
        }
        let batch = shared.store.take_outbox();
        if !batch.is_empty() && shared.enqueue(batch).await.is_err() {
            return;
        }
        let last = shared.last_id.load(Ordering::SeqCst);
        for d in waiting {
            let _ = d.send(last);
        }
    }
}

async fn work(shared: Arc<Shared>) {
    while let Ok(permit) = shared.items.acquire().await {
        permit.forget();
        let Some(job) = shared.take_job() else {
            shared.notify();
            continue;
        };
        shared.notify();
        shared.attempt(job).await;
        shared.notify();
    }
}
