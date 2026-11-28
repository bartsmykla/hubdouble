use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use bytes::Bytes;
use hubdouble_model::{Clock, Delivery, DeliveryRequest, DeliveryResponse, Timestamp};
use hubdouble_store::{
    Apps, Committed, Counters, Deliveries, OutboxEntry, OutboxUse, Sequence, Snapshot, Store, Tx,
};
use parking_lot::Mutex;
use tokio::sync::{Semaphore, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::delivery::{Outcome, Pool, send, signature_1, signature_256};
use crate::{ContentType, Hooks, HooksError, Notice, Options, QUEUE_BYTES, Stats};

const OVER_BUDGET: &str = "Queue full";

pub(crate) struct Job {
    id: i64,
    epoch: u64,
    log: i64,
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
    wire: Bytes,
    over_budget: bool,
}

impl Job {
    fn logged(self, delivered_at: Timestamp, duration: f64, outcome: Outcome) -> Delivery {
        Delivery {
            id: self.id,
            guid: self.guid,
            app_id: self.app_id,
            hook_id: self.log,
            installation_id: self.installation_id,
            repo_id: self.repo_id,
            event: self.event,
            action: self.action,
            url: self.url,
            delivered_at,
            redelivery: self.redelivery,
            duration,
            status: outcome.status,
            status_code: outcome.status_code,
            request: DeliveryRequest {
                headers: self.headers,
                payload: self.body,
            },
            response: DeliveryResponse {
                headers: outcome.headers,
                payload: outcome.body,
            },
        }
    }
}

struct Target {
    log: i64,
    app_id: i64,
    url: String,
    secret: Option<String>,
    form: bool,
    kind: &'static str,
    owner_id: i64,
}

impl Target {
    fn of(tx: &Tx, entry: &OutboxEntry) -> Option<Target> {
        if let Some(id) = entry.hook_id {
            let hook = tx.try_table::<Hooks>()?.get(id).filter(|h| h.active)?;
            return Some(Target {
                log: id,
                app_id: 0,
                url: hook.url.clone(),
                secret: hook.secret.clone(),
                form: hook.content_type == ContentType::Form,
                kind: hook.owner.target_type(),
                owner_id: hook.owner.id(),
            });
        }
        let app = tx.table::<Apps>().get(entry.app_id)?;
        let hook = app.webhook.as_ref().filter(|w| w.active)?;
        Some(Target {
            log: app.id,
            app_id: app.id,
            url: hook.url.clone(),
            secret: hook.secret.clone(),
            form: false,
            kind: "integration",
            owner_id: app.id,
        })
    }
}

struct Assigned {
    id: i64,
    target: Target,
    committed: Committed,
}

#[derive(Default)]
struct AppLog {
    running: BTreeSet<i64>,
    ready: BTreeMap<i64, (u64, Delivery)>,
    logged: Option<VecDeque<i64>>,
}

fn header_bytes(h: &BTreeMap<String, String>) -> usize {
    h.iter().map(|(k, v)| k.len() + v.len()).sum()
}

fn body_bytes(d: &Delivery) -> usize {
    d.request.payload.len()
        + header_bytes(&d.request.headers)
        + d.response.payload.as_ref().map_or(0, String::len)
        + header_bytes(&d.response.headers)
}

fn strip(d: &mut Delivery) {
    d.request.payload = Bytes::new();
    d.request.headers = BTreeMap::new();
    d.response.payload = None;
    d.response.headers = BTreeMap::new();
}

#[derive(Default)]
struct Bodies {
    held: usize,
    kept: BTreeMap<i64, (i64, usize)>,
}

impl Bodies {
    fn hold(&mut self, app: i64, d: &Delivery) {
        let bytes = body_bytes(d);
        self.held += bytes;
        if let Some((_, old)) = self.kept.insert(d.id, (app, bytes)) {
            self.held -= old;
        }
    }

    fn release(&mut self, id: i64) {
        if let Some((_, bytes)) = self.kept.remove(&id) {
            self.held -= bytes;
        }
    }

    fn release_log(&mut self, log: i64) {
        let held = &mut self.held;
        self.kept.retain(|_, (app, bytes)| {
            let keep = *app != log;
            if !keep {
                *held -= *bytes;
            }
            keep
        });
    }
}

fn owner_exists(tx: &Tx, log: i64) -> bool {
    tx.try_table::<Hooks>()
        .is_some_and(|h| h.get(log).is_some())
        || tx.table::<Apps>().get(log).is_some()
}

#[derive(Default)]
struct State {
    queue: VecDeque<Job>,
    apps: HashMap<i64, AppLog>,
    bodies: Bodies,
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
    pub(crate) pool: Pool,
}

impl Shared {
    pub(crate) fn start(
        store: Store,
        clock: Arc<dyn Clock>,
        options: Options,
    ) -> Result<Arc<Shared>, HooksError> {
        let rt = tokio::runtime::Handle::try_current().map_err(|_| HooksError::NoRuntime)?;
        store.limit_outbox(OutboxUse {
            events: options.queue_capacity,
            bytes: match options.queue_bytes {
                0 => QUEUE_BYTES,
                n => n,
            },
            refused_bytes: options.log_bytes,
            refused_per_app: options.log_capacity.max(1),
        });
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
            pool: Pool::new(options.workers.max(1)),
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
        let epoch = self.store.restore_epoch(snapshot);
        self.forget_older(epoch).await?;
        self.forget_discarded_hooks();
        Ok(())
    }

    fn forget_discarded_hooks(&self) {
        let mut st = self.state.lock();
        let Ok(doomed) = self.store.update(|tx| {
            let hooks = tx.try_table::<Hooks>();
            let apps = tx.table::<Apps>();
            let doomed: Vec<i64> = tx
                .table::<Deliveries>()
                .iter()
                .filter(|d| {
                    if d.app_id == 0 {
                        hooks.is_none_or(|h| h.get(d.hook_id).is_none())
                    } else {
                        apps.get(d.app_id).is_none()
                    }
                })
                .map(|d| d.id)
                .collect();
            tx.table_mut::<Deliveries>()
                .remove_all(doomed.iter().copied());
            Ok(doomed)
        }) else {
            return;
        };
        for id in doomed {
            st.bodies.release(id);
        }
        self.forget_gone(&mut st);
    }

    fn forget_gone(&self, st: &mut State) {
        let Ok(gone) = self.store.view(|tx| {
            Ok(st
                .apps
                .iter()
                .filter(|(log, _)| !owner_exists(tx, **log))
                .map(|(log, _)| *log)
                .collect::<Vec<i64>>())
        }) else {
            return;
        };
        for log in gone {
            st.bodies.release_log(log);
            if st.apps.get(&log).is_some_and(|l| l.running.is_empty()) {
                st.apps.remove(&log);
            }
        }
    }

    pub(crate) async fn forget_older(&self, epoch: u64) -> Result<(), HooksError> {
        {
            let mut st = self.state.lock();
            let (dropped, kept): (Vec<Job>, Vec<Job>) =
                st.queue.drain(..).partition(|job| job.epoch < epoch);
            st.queue.extend(kept);
            self.space.add_permits(dropped.len());
            for job in dropped {
                self.release_job(&job);
                self.finish(&mut st, job.log, job.id, None);
            }
            let stale: Vec<(i64, i64)> = st
                .apps
                .iter()
                .flat_map(|(app, log)| {
                    log.ready
                        .iter()
                        .filter(|(_, (e, _))| *e < epoch)
                        .map(|(id, _)| (*app, *id))
                })
                .collect();
            for (app, id) in stale {
                if let Some(log) = st.apps.get_mut(&app) {
                    log.ready.remove(&id);
                }
                st.bodies.release(id);
                st.unlogged.remove(&id);
            }
        }
        self.notify();
        self.wait_until(|st| st.in_flight.range(..epoch).next().is_none())
            .await
    }

    #[cfg(test)]
    pub(crate) async fn wait_stats(&self, f: impl Fn(&Stats) -> bool) {
        let _ = self.wait_until(|st| f(&self.stats_of(st))).await;
    }

    pub(crate) fn stats(&self) -> Stats {
        self.stats_of(&self.state.lock())
    }

    fn stats_of(&self, st: &State) -> Stats {
        Stats {
            queued: st.queue.len(),
            max_queued: st.max_queued,
            in_flight: st.in_flight.values().sum(),
            unlogged: st.unlogged.len(),
            waiting_bytes: self.store.outbox_use().bytes,
            log_bytes: st.bodies.held,
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

    fn assign(&self, batch: Vec<Committed>) -> Vec<Assigned> {
        self.store
            .update(|tx| {
                let epoch = self.store.epoch();
                let mut out = Vec::with_capacity(batch.len());
                for c in batch {
                    let target = (c.epoch == epoch)
                        .then(|| Target::of(tx, &c.entry))
                        .flatten();
                    let Some(target) = target else {
                        self.release(&c);
                        continue;
                    };
                    let id = tx.table_mut::<Counters>().next(Sequence::Delivery);
                    out.push(Assigned {
                        id,
                        target,
                        committed: c,
                    });
                }
                Ok(out)
            })
            .unwrap_or_default()
    }

    fn release_bytes(&self, over_budget: bool, bytes: usize) {
        if over_budget {
            self.store.release_refused(bytes);
        } else {
            self.store.release_outbox(bytes);
        }
    }

    fn release(&self, c: &Committed) {
        self.release_bytes(c.over_budget, c.entry.payload.len());
    }

    fn release_job(&self, job: &Job) {
        self.release_bytes(job.over_budget, job.body.len());
    }

    fn job(a: Assigned) -> Job {
        let Assigned {
            id,
            target,
            committed,
        } = a;
        let epoch = committed.epoch;
        let over_budget = committed.over_budget;
        let entry = committed.entry;
        let event = entry.event.into_owned();
        let action = entry.action.map(Cow::into_owned);
        let (guid, redelivery) = match entry.redelivery_of {
            Some(guid) => (guid, true),
            None => (uuid::Uuid::new_v4().to_string(), false),
        };
        let body = entry.payload;
        let (content_type, wire) = if target.form {
            let mut form = b"payload=".to_vec();
            form.extend(url::form_urlencoded::byte_serialize(&body).flat_map(str::bytes));
            ("application/x-www-form-urlencoded", Bytes::from(form))
        } else {
            ("application/json", body.clone())
        };
        let mut headers = BTreeMap::from([
            ("Accept".to_owned(), "*/*".to_owned()),
            ("Content-Type".to_owned(), content_type.to_owned()),
            (
                "User-Agent".to_owned(),
                "GitHub-Hookshot/hubdouble".to_owned(),
            ),
            ("X-GitHub-Delivery".to_owned(), guid.clone()),
            ("X-GitHub-Event".to_owned(), event.clone()),
            ("X-GitHub-Hook-ID".to_owned(), target.log.to_string()),
            (
                "X-GitHub-Hook-Installation-Target-ID".to_owned(),
                target.owner_id.to_string(),
            ),
            (
                "X-GitHub-Hook-Installation-Target-Type".to_owned(),
                target.kind.to_owned(),
            ),
        ]);
        if let Some(secret) = target.secret.as_deref().filter(|s| !s.is_empty()) {
            headers.insert(
                "X-Hub-Signature".to_owned(),
                signature_1(secret.as_bytes(), &wire),
            );
            headers.insert(
                "X-Hub-Signature-256".to_owned(),
                signature_256(secret.as_bytes(), &wire),
            );
        }
        Job {
            id,
            epoch,
            log: target.log,
            app_id: target.app_id,
            installation_id: entry.installation_id,
            repo_id: entry.repo_id,
            event,
            action,
            guid,
            redelivery,
            url: target.url,
            headers,
            body,
            wire,
            over_budget,
        }
    }

    async fn enqueue(&self, batch: Vec<Committed>) -> Result<(), HooksError> {
        let assigned = self.assign(batch);
        let Some(last) = assigned.last() else {
            return Ok(());
        };
        if let Some(observe) = &self.options.observer {
            for a in &assigned {
                observe(&Notice::Event {
                    app_id: a.target.app_id,
                    delivery_id: a.id,
                    event: &a.committed.entry.event,
                    action: a.committed.entry.action.as_deref(),
                });
            }
        }
        self.last_id.store(last.id, Ordering::SeqCst);
        let jobs: Vec<Job> = assigned.into_iter().map(Shared::job).collect();
        {
            let mut st = self.state.lock();
            for job in &jobs {
                st.unlogged.insert(job.id);
                st.apps.entry(job.log).or_default().running.insert(job.id);
            }
        }
        for job in jobs {
            if job.over_budget {
                self.refuse(job);
                continue;
            }
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
                self.release_job(&job);
                self.finish(&mut st, job.log, job.id, None);
            }
        }
        self.notify();
        Ok(())
    }

    fn refuse(&self, job: Job) {
        let (app, id, epoch) = (job.log, job.id, job.epoch);
        let bytes = job.body.len();
        let mut delivery = job.logged(self.clock.now(), 0.0, Outcome::failed(OVER_BUDGET));
        if bytes == 0 {
            strip(&mut delivery);
        }
        let mut st = self.state.lock();
        let logged = (epoch == self.store.epoch()).then_some((epoch, delivery));
        self.finish(&mut st, app, id, logged);
        drop(st);
        self.release_bytes(true, bytes);
    }

    fn take_job(&self) -> Option<Job> {
        let mut st = self.state.lock();
        let job = st.queue.pop_front()?;
        self.space.add_permits(1);
        if job.epoch != self.store.epoch() {
            self.release_job(&job);
            self.finish(&mut st, job.log, job.id, None);
            return None;
        }
        *st.in_flight.entry(job.epoch).or_default() += 1;
        st.bodies.held += job.body.len() + header_bytes(&job.headers);
        self.prune(&mut st);
        Some(job)
    }

    async fn attempt(&self, job: Job) {
        let (app, id, epoch) = (job.log, job.id, job.epoch);
        let sent = job.body.len() + header_bytes(&job.headers);
        let delivered_at = self.clock.now();
        let started = Instant::now();
        let outcome = tokio::time::timeout(
            self.options.timeout,
            send(
                &*self.options.connector,
                &self.pool,
                &job.url,
                &job.headers,
                job.wire.clone(),
                self.options.response_bytes,
            ),
        )
        .await
        .unwrap_or_else(|_| Outcome::failed("Timed out"));
        self.release_job(&job);
        let delivery = job.logged(delivered_at, started.elapsed().as_secs_f64(), outcome);
        let mut st = self.state.lock();
        st.bodies.held -= sent;
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
                st.bodies.hold(app, &d.1);
                log.ready.insert(id, d);
            }
            None => {
                st.unlogged.remove(&id);
            }
        }
        self.flush(st, app);
        self.forget_gone(st);
        self.prune(st);
    }

    fn prune(&self, st: &mut State) {
        let limit = self.options.log_bytes;
        if st.bodies.held > limit {
            self.forget_gone(st);
        }
        let mut logged = Vec::new();
        while st.bodies.held > limit {
            let Some((id, (app, bytes))) = st.bodies.kept.pop_first() else {
                break;
            };
            st.bodies.held -= bytes;
            match st.apps.get_mut(&app).and_then(|log| log.ready.get_mut(&id)) {
                Some((_, d)) => strip(d),
                None => logged.push(id),
            }
        }
        if logged.is_empty() {
            return;
        }
        let _ = self.store.update(|tx| {
            let mut table = tx.table_mut::<Deliveries>();
            for id in logged {
                if let Some(d) = table.get(id) {
                    let mut d = (**d).clone();
                    strip(&mut d);
                    table.replace(d)?;
                }
            }
            Ok(())
        });
    }

    fn flush(&self, st: &mut State, app: i64) {
        let State {
            apps,
            unlogged,
            bodies,
            ..
        } = st;
        let Some(log) = apps.get_mut(&app) else {
            return;
        };
        let capacity = self.options.log_capacity.max(1);
        let floor = log.running.first().copied().unwrap_or(i64::MAX);
        let mut rest = log.ready.split_off(&floor);
        while rest.len() > capacity
            && let Some((id, _)) = rest.pop_first()
        {
            bodies.release(id);
            unlogged.remove(&id);
        }
        let ready = std::mem::replace(&mut log.ready, rest);
        if ready.is_empty() {
            return;
        }
        let logged = &mut log.logged;
        let mut inserted = Vec::new();
        let mut gone = Vec::new();
        let _ = self.store.update(|tx| {
            let epoch = self.store.epoch();
            let owner_exists = owner_exists(tx, app);
            let logged = logged.get_or_insert_with(|| {
                tx.table::<Deliveries>()
                    .children(app)
                    .map(|d| d.id)
                    .collect()
            });
            let mut table = tx.table_mut::<Deliveries>();
            for (e, d) in ready.values() {
                if *e == epoch && owner_exists && table.insert(d.clone()).is_ok() {
                    logged.push_back(d.id);
                    inserted.push(d);
                } else {
                    gone.push(d.id);
                }
                while logged.len() > capacity {
                    if let Some(old) = logged.pop_front() {
                        table.remove(old);
                        gone.push(old);
                    }
                }
            }
            Ok(())
        });
        for id in gone {
            bodies.release(id);
        }
        if let Some(observe) = &self.options.observer {
            for d in inserted {
                observe(&Notice::Delivery {
                    app_id: d.app_id,
                    delivery_id: d.id,
                    event: &d.event,
                    action: d.action.as_deref(),
                    status_code: d.status_code,
                });
            }
        }
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
