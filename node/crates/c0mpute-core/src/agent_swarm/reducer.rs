//! The swarm state machine: a pure fold over a swarm's events.
//!
//! Events are applied in `(lamport, id)` order, so every member that holds
//! the same events computes the same state, whatever order they arrived in.
//! An event that breaks a rule at its place in that order (a claim on a task
//! someone else holds, a submit without a claim, a close by a stranger) is
//! skipped, not an error: it stays in the log, it just has no effect.
//!
//! Rules (DIP 0034):
//! * A task is blocked until every task it `needs` is done; the final task
//!   is blocked until every other task of its job is done.
//! * A claim holds a task for its lease; others cannot claim it meanwhile.
//! * Only someone who claimed a task may submit to it.
//! * Nobody reviews their own submission. A job owner's vote decides; with
//!   no owner vote, a result stands when it has at least one endorsement and
//!   no more challenges than endorsements.
//! * A task is done while one of its results stands; a job is complete when
//!   its final task is done.
//! * A bounty is payable when an owner accepted the result, or at least two
//!   reviewers with standing (someone else once paid them) endorsed it and
//!   they outnumber challenges.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use sha2::{Digest, Sha256};

use super::event::{Body, Event, FINAL_KEY, MAX_TASKS_PER_JOB, Payee, TaskSpec, Verdict, make_task_id};
use crate::payments::{Micros, parse_usd};

#[derive(Clone, Debug, Serialize)]
pub struct Job {
    pub id: String,
    pub author: String,
    pub title: String,
    pub desc: String,
    /// Per accepted task, micro-USD (0 = unpaid).
    pub bounty: Micros,
    /// Author plus declared owners.
    pub owners: Vec<String>,
    pub closed: bool,
    pub ts: u64,
    /// Task keys in the order added; `final` is always last.
    pub tasks: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Claim {
    pub holder: String,
    pub until: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Task {
    pub id: String,
    pub job: String,
    pub key: String,
    pub title: String,
    pub desc: String,
    pub needs: Vec<String>,
    pub is_final: bool,
    pub claim: Option<Claim>,
    pub claimed_by: BTreeSet<String>,
    pub submissions: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Review {
    pub verdict: Verdict,
    pub note: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Paid {
    pub amount_usd: String,
    pub rail_ref: String,
    pub by: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Submission {
    pub id: String,
    pub task: String,
    pub author: String,
    pub content: String,
    pub payee: Option<Payee>,
    pub ts: u64,
    pub reviews: BTreeMap<String, Review>,
    pub paid: Option<Paid>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MsgView {
    pub id: String,
    pub author: String,
    pub job: Option<String>,
    pub text: String,
    pub ts: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct State {
    pub jobs: BTreeMap<String, Job>,
    pub tasks: BTreeMap<String, Task>,
    pub submissions: BTreeMap<String, Submission>,
    pub messages: Vec<MsgView>,
    /// Events that had no effect.
    pub skipped: usize,
    pub events: usize,
}

/// Messages kept in state (the log keeps them all).
const MAX_MESSAGES: usize = 500;

/// What a worker should do next.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum Next {
    Review { submission: String },
    Plan { job: String },
    Work { task: String },
    Idle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Done,
    Blocked,
    Claimed,
    Review,
    Open,
    Closed,
}

impl State {
    /// Fold `events` (any order; sorted here) into a state.
    pub fn build<'a>(events: impl IntoIterator<Item = &'a Event>) -> Self {
        let mut evs: Vec<&Event> = events.into_iter().collect();
        evs.sort_by(|a, b| (a.lamport, &a.id).cmp(&(b.lamport, &b.id)));
        let mut s = State::default();
        for e in evs {
            s.events += 1;
            let applied = match Body::parse(&e.kind, &e.body) {
                Ok(b) => s.apply(e, b),
                Err(_) => false,
            };
            if !applied {
                s.skipped += 1;
            }
        }
        s
    }

    fn apply(&mut self, e: &Event, body: Body) -> bool {
        match body {
            Body::JobPost(p) => {
                if self.jobs.contains_key(&e.id) {
                    return false;
                }
                // Needs refer to tasks earlier in the list: no cycles.
                let mut seen = BTreeSet::new();
                for t in &p.tasks {
                    if t.needs.iter().any(|n| !seen.contains(n)) {
                        return false;
                    }
                    seen.insert(t.key.clone());
                }
                let mut owners = vec![e.author.clone()];
                for o in p.owners {
                    if !owners.contains(&o) {
                        owners.push(o);
                    }
                }
                let job = Job {
                    id: e.id.clone(),
                    author: e.author.clone(),
                    title: p.title,
                    desc: p.desc,
                    bounty: if p.bounty_usd.is_empty() { 0 } else { parse_usd(&p.bounty_usd).unwrap_or(0) },
                    owners,
                    closed: false,
                    ts: e.ts,
                    tasks: Vec::new(),
                };
                self.jobs.insert(e.id.clone(), job);
                for t in &p.tasks {
                    self.insert_task(&e.id, t, false);
                }
                let fin = TaskSpec {
                    key: FINAL_KEY.into(),
                    title: "Merge everything into the final deliverable".into(),
                    desc: String::new(),
                    needs: vec![],
                };
                self.insert_task(&e.id, &fin, true);
                true
            }
            Body::TaskAdd(a) => {
                let Some(job) = self.jobs.get(&a.job) else { return false };
                if job.closed || self.job_complete(&a.job) || job.tasks.len() >= MAX_TASKS_PER_JOB {
                    return false;
                }
                if job.tasks.contains(&a.key) || a.needs.iter().any(|n| !job.tasks.contains(n) || n == FINAL_KEY) {
                    return false;
                }
                let job_id = a.job.clone();
                self.insert_task(&job_id, &a.spec(), false);
                true
            }
            Body::TaskClaim(c) => {
                let Some(t) = self.tasks.get(&c.task) else { return false };
                if !self.job_open(&t.job) || self.task_done(&c.task) || self.task_blocked(&c.task) {
                    return false;
                }
                if let Some(cl) = &t.claim
                    && cl.holder != e.author
                    && cl.until > e.ts
                {
                    return false;
                }
                let t = self.tasks.get_mut(&c.task).expect("checked");
                t.claim = Some(Claim {
                    holder: e.author.clone(),
                    until: e.ts.saturating_add(c.lease_secs),
                });
                t.claimed_by.insert(e.author.clone());
                true
            }
            Body::TaskRelease(r) => {
                let Some(t) = self.tasks.get_mut(&r.task) else { return false };
                if t.claim.as_ref().is_some_and(|c| c.holder == e.author) {
                    t.claim = None;
                    true
                } else {
                    false
                }
            }
            Body::TaskSubmit(s) => {
                let Some(t) = self.tasks.get(&s.task) else { return false };
                if !self.job_open(&t.job) || !t.claimed_by.contains(&e.author) || self.task_done(&s.task) {
                    return false;
                }
                let t = self.tasks.get_mut(&s.task).expect("checked");
                if t.claim.as_ref().is_some_and(|c| c.holder == e.author) {
                    t.claim = None;
                }
                t.submissions.push(e.id.clone());
                self.submissions.insert(
                    e.id.clone(),
                    Submission {
                        id: e.id.clone(),
                        task: s.task,
                        author: e.author.clone(),
                        content: s.content,
                        payee: s.payee,
                        ts: e.ts,
                        reviews: BTreeMap::new(),
                        paid: None,
                    },
                );
                true
            }
            Body::TaskReview(r) => {
                let Some(sub) = self.submissions.get_mut(&r.submission) else { return false };
                if sub.author == e.author {
                    return false;
                }
                sub.reviews.insert(
                    e.author.clone(),
                    Review {
                        verdict: r.verdict,
                        note: r.note,
                    },
                );
                true
            }
            Body::TaskPaid(p) => {
                let Some(sub) = self.submissions.get(&p.submission) else { return false };
                let Some(job) = self.tasks.get(&sub.task).and_then(|t| self.jobs.get(&t.job)) else {
                    return false;
                };
                if !job.owners.contains(&e.author) {
                    return false;
                }
                let sub = self.submissions.get_mut(&p.submission).expect("checked");
                sub.paid = Some(Paid {
                    amount_usd: p.amount_usd,
                    rail_ref: p.rail_ref,
                    by: e.author.clone(),
                });
                true
            }
            Body::JobClose(c) => match self.jobs.get_mut(&c.job) {
                Some(j) if j.owners.contains(&e.author) && !j.closed => {
                    j.closed = true;
                    true
                }
                _ => false,
            },
            Body::Msg(m) => {
                if m.job.as_ref().is_some_and(|j| !self.jobs.contains_key(j)) {
                    return false;
                }
                self.messages.push(MsgView {
                    id: e.id.clone(),
                    author: e.author.clone(),
                    job: m.job,
                    text: m.text,
                    ts: e.ts,
                });
                let over = self.messages.len().saturating_sub(MAX_MESSAGES);
                self.messages.drain(..over);
                true
            }
        }
    }

    fn insert_task(&mut self, job: &str, t: &TaskSpec, is_final: bool) {
        let id = make_task_id(job, &t.key);
        let j = self.jobs.get_mut(job).expect("job exists");
        if is_final {
            j.tasks.push(t.key.clone());
        } else {
            // Keep `final` last.
            let at = j.tasks.iter().position(|k| k == FINAL_KEY).unwrap_or(j.tasks.len());
            j.tasks.insert(at, t.key.clone());
        }
        self.tasks.insert(
            id.clone(),
            Task {
                id,
                job: job.to_string(),
                key: t.key.clone(),
                title: t.title.clone(),
                desc: t.desc.clone(),
                needs: t.needs.iter().map(|n| make_task_id(job, n)).collect(),
                is_final,
                claim: None,
                claimed_by: BTreeSet::new(),
                submissions: Vec::new(),
            },
        );
    }

    fn job_open(&self, job: &str) -> bool {
        self.jobs.get(job).is_some_and(|j| !j.closed)
    }

    fn owner_votes(&self, sub: &Submission) -> (bool, bool) {
        let owners = self
            .tasks
            .get(&sub.task)
            .and_then(|t| self.jobs.get(&t.job))
            .map(|j| j.owners.clone())
            .unwrap_or_default();
        let mut acc = false;
        let mut rej = false;
        for (who, r) in &sub.reviews {
            if owners.contains(who) {
                match r.verdict {
                    Verdict::Accept => acc = true,
                    Verdict::Reject => rej = true,
                }
            }
        }
        (acc, rej)
    }

    fn tally(sub: &Submission) -> (usize, usize) {
        let e = sub.reviews.values().filter(|r| r.verdict == Verdict::Accept).count();
        (e, sub.reviews.len() - e)
    }

    /// Whether a result currently stands.
    pub fn stands(&self, sub: &Submission) -> bool {
        match self.owner_votes(sub) {
            (_, true) => false,
            (true, false) => true,
            (false, false) => {
                let (e, c) = Self::tally(sub);
                e >= 1 && e >= c
            }
        }
    }

    /// Whether `peer` has standing as a reviewer: someone other than itself
    /// marked one of its results paid. Paying costs real money, so a swarm
    /// of fresh identities endorsing each other has none.
    pub fn has_standing(&self, peer: &str) -> bool {
        self.submissions
            .values()
            .any(|s| s.author == peer && s.paid.as_ref().is_some_and(|p| p.by != peer))
    }

    /// Whether the job's bounty is owed for this result: an owner accepted
    /// it, or at least two reviewers with standing endorsed it and they
    /// outnumber every challenge.
    pub fn payable(&self, sub: &Submission) -> bool {
        match self.owner_votes(sub) {
            (_, true) => false,
            (true, false) => true,
            (false, false) => {
                let trusted = sub
                    .reviews
                    .iter()
                    .filter(|(who, r)| r.verdict == Verdict::Accept && self.has_standing(who))
                    .count();
                let (_, c) = Self::tally(sub);
                trusted >= 2 && trusted > c
            }
        }
    }

    pub fn task_done(&self, task: &str) -> bool {
        self.tasks.get(task).is_some_and(|t| {
            t.submissions
                .iter()
                .filter_map(|s| self.submissions.get(s))
                .any(|s| self.stands(s))
        })
    }

    pub fn task_blocked(&self, task: &str) -> bool {
        let Some(t) = self.tasks.get(task) else { return true };
        if t.is_final {
            let Some(job) = self.jobs.get(&t.job) else { return true };
            return job
                .tasks
                .iter()
                .filter(|k| *k != FINAL_KEY)
                .any(|k| !self.task_done(&make_task_id(&t.job, k)));
        }
        t.needs.iter().any(|n| !self.task_done(n))
    }

    pub fn job_complete(&self, job: &str) -> bool {
        self.task_done(&make_task_id(job, FINAL_KEY))
    }

    /// The final deliverable, once the job is complete.
    pub fn job_result(&self, job: &str) -> Option<&Submission> {
        let t = self.tasks.get(&make_task_id(job, FINAL_KEY))?;
        t.submissions
            .iter()
            .filter_map(|s| self.submissions.get(s))
            .find(|s| self.stands(s))
    }

    pub fn task_status(&self, task: &str, now: u64) -> TaskStatus {
        let Some(t) = self.tasks.get(task) else { return TaskStatus::Closed };
        if self.task_done(task) {
            return TaskStatus::Done;
        }
        if !self.job_open(&t.job) {
            return TaskStatus::Closed;
        }
        if self.task_blocked(task) {
            return TaskStatus::Blocked;
        }
        if t.claim.as_ref().is_some_and(|c| c.until > now) {
            return TaskStatus::Claimed;
        }
        if !t.submissions.is_empty()
            && t
                .submissions
                .iter()
                .filter_map(|s| self.submissions.get(s))
                .any(|s| !self.owner_votes(s).1 && Self::tally(s).1 <= Self::tally(s).0)
        {
            return TaskStatus::Review;
        }
        TaskStatus::Open
    }

    /// Jobs that have no tasks besides the final one: someone should plan.
    fn needs_plan(&self, job: &Job) -> bool {
        job.tasks.iter().all(|k| k == FINAL_KEY)
    }

    /// What `me` should do next, considering only jobs `allow` admits.
    /// Picks among equals by `sha256(me || id)`, so workers spread out
    /// instead of all reaching for the same task.
    pub fn next_for(&self, me: &str, now: u64, allow: impl Fn(&Job) -> bool) -> Next {
        let spread = |id: &str| -> [u8; 32] {
            let mut h = Sha256::new();
            h.update(me.as_bytes());
            h.update(id.as_bytes());
            h.finalize().into()
        };
        let live_jobs: Vec<&Job> = self
            .jobs
            .values()
            .filter(|j| !j.closed && !self.job_complete(&j.id) && allow(j))
            .collect();
        let live: BTreeSet<&str> = live_jobs.iter().map(|j| j.id.as_str()).collect();

        // 1. Review: results on unfinished tasks that I did not write or
        // review yet. Reviews are what turn a submission into done work.
        let mut reviews: Vec<&Submission> = self
            .submissions
            .values()
            .filter(|s| s.author != me && !s.reviews.contains_key(me))
            .filter(|s| {
                self.tasks
                    .get(&s.task)
                    .is_some_and(|t| live.contains(t.job.as_str()) && !self.task_done(&t.id))
            })
            .filter(|s| !self.owner_votes(s).1)
            // Never auto-review a paid job I own: an owner's accept is what
            // makes a submission payable, and the reviewer here is a model
            // reading text the submitter wrote ("first line: ACCEPT"). The
            // owner accepts by hand; other nodes still review it.
            .filter(|s| {
                !self
                    .tasks
                    .get(&s.task)
                    .and_then(|t| self.jobs.get(&t.job))
                    .is_some_and(|j| j.bounty > 0 && j.owners.iter().any(|o| o == me))
            })
            .collect();
        reviews.sort_by_key(|s| (s.reviews.len(), spread(&s.id)));
        if let Some(s) = reviews.first() {
            return Next::Review { submission: s.id.clone() };
        }

        // 2. Plan: jobs nobody split yet, whose final task is free.
        let mut plans: Vec<&&Job> = live_jobs
            .iter()
            .filter(|j| self.needs_plan(j))
            .filter(|j| {
                let fin = make_task_id(&j.id, FINAL_KEY);
                self.tasks
                    .get(&fin)
                    .is_some_and(|t| t.claim.as_ref().is_none_or(|c| c.until <= now || c.holder == me))
                    && self.tasks.get(&fin).is_some_and(|t| t.submissions.is_empty())
            })
            .collect();
        plans.sort_by_key(|j| spread(&j.id));
        if let Some(j) = plans.first() {
            return Next::Plan { job: j.id.clone() };
        }

        // 3. Work: open, unblocked tasks I have no live result on.
        let mut work: Vec<&Task> = self
            .tasks
            .values()
            .filter(|t| live.contains(t.job.as_str()))
            .filter(|t| !self.task_done(&t.id) && !self.task_blocked(&t.id))
            .filter(|t| t.claim.as_ref().is_none_or(|c| c.until <= now || c.holder == me))
            .filter(|t| {
                !t.submissions
                    .iter()
                    .filter_map(|s| self.submissions.get(s))
                    .any(|s| s.author == me && !self.owner_votes(s).1 && Self::tally(s).1 <= Self::tally(s).0)
            })
            .filter(|t| !(t.is_final && self.jobs.get(&t.job).is_some_and(|j| self.needs_plan(j)) && t.claim.is_none()))
            .collect();
        work.sort_by_key(|t| (t.is_final, spread(&t.id)));
        if let Some(t) = work.first() {
            return Next::Work { task: t.id.clone() };
        }
        Next::Idle
    }

    /// Results owed a bounty that `payer` (an owner) has not marked paid.
    pub fn owed_by<'a>(&'a self, owners: &'a [String]) -> impl Iterator<Item = (&'a Job, &'a Submission)> + 'a {
        self.submissions.values().filter_map(move |s| {
            let t = self.tasks.get(&s.task)?;
            let j = self.jobs.get(&t.job)?;
            (j.bounty > 0 && s.paid.is_none() && j.owners.iter().any(|o| owners.contains(o)) && self.payable(s))
                .then_some((j, s))
        })
    }

    /// Accepted-but-unpaid results older than `grace` seconds for jobs
    /// `poster` wrote: how a worker tells a poster who does not pay.
    pub fn unpaid_count(&self, poster: &str, now: u64, grace: u64) -> usize {
        self.submissions
            .values()
            .filter(|s| s.paid.is_none() && s.ts + grace < now && self.payable(s))
            .filter(|s| {
                self.tasks
                    .get(&s.task)
                    .and_then(|t| self.jobs.get(&t.job))
                    .is_some_and(|j| j.author == poster && j.bounty > 0)
            })
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::super::event::tests::{ev, signer};
    use super::super::event::*;
    use super::*;

    const SW: &str = "sw";

    fn post(tasks: Vec<(&str, Vec<&str>)>, bounty: &str) -> Body {
        Body::JobPost(JobPost {
            title: "Build it".into(),
            desc: String::new(),
            tasks: tasks
                .into_iter()
                .map(|(k, n)| TaskSpec {
                    key: k.into(),
                    title: k.into(),
                    desc: String::new(),
                    needs: n.into_iter().map(String::from).collect(),
                })
                .collect(),
            bounty_usd: bounty.into(),
            owners: vec![],
        })
    }

    fn claim(t: &str) -> Body {
        Body::TaskClaim(TaskClaim { task: t.into(), lease_secs: 60 })
    }
    fn submit(t: &str, c: &str) -> Body {
        Body::TaskSubmit(TaskSubmit { task: t.into(), content: c.into(), payee: None })
    }
    fn review(s: &str, ok: bool) -> Body {
        Body::TaskReview(TaskReview {
            submission: s.into(),
            verdict: if ok { Verdict::Accept } else { Verdict::Reject },
            note: String::new(),
        })
    }

    #[test]
    fn full_job_lifecycle() {
        let (poster, pp) = signer();
        let (w1, p1) = signer();
        let (w2, p2) = signer();
        let me1 = p1.to_string();
        let me2 = p2.to_string();
        let job = ev(&poster, SW, 1, 100, post(vec![("a", vec![]), ("b", vec!["a"])], "0.05"));
        let a = make_task_id(&job.id, "a");
        let b = make_task_id(&job.id, "b");
        let fin = make_task_id(&job.id, FINAL_KEY);
        let mut log = vec![job.clone()];

        let s = State::build(&log);
        assert!(s.task_blocked(&b), "b needs a");
        assert!(s.task_blocked(&fin), "final waits for all");
        assert_eq!(s.next_for(&me1, 100, |_| true), Next::Work { task: a.clone() });

        // w1 claims a; w2 cannot claim it during the lease.
        log.push(ev(&w1, SW, 2, 101, claim(&a)));
        log.push(ev(&w2, SW, 3, 102, claim(&a)));
        let s = State::build(&log);
        assert_eq!(s.tasks[&a].claim.as_ref().unwrap().holder, me1);
        // w2 without a claim cannot submit.
        log.push(ev(&w2, SW, 4, 103, submit(&a, "drive-by")));
        let sub_a = ev(&w1, SW, 5, 104, submit(&a, "A done"));
        log.push(sub_a.clone());
        let s = State::build(&log);
        assert_eq!(s.tasks[&a].submissions, vec![sub_a.id.clone()]);
        assert!(!s.task_done(&a), "needs a review");
        assert_eq!(s.next_for(&me2, 105, |_| true), Next::Review { submission: sub_a.id.clone() });
        // The owner of a paid job never auto-reviews it: its accept pays.
        assert!(!matches!(s.next_for(&pp.to_string(), 105, |_| true), Next::Review { .. }));
        assert_eq!(s.next_for(&me1, 105, |_| true), Next::Idle, "own submission is not mine to review");

        // Self-review has no effect; w2 endorses.
        log.push(ev(&w1, SW, 6, 105, review(&sub_a.id, true)));
        log.push(ev(&w2, SW, 7, 106, review(&sub_a.id, true)));
        let s = State::build(&log);
        assert!(s.task_done(&a));
        assert!(!s.payable(&s.submissions[&sub_a.id]), "one peer endorsement is not enough to pay");
        assert!(!s.task_blocked(&b));

        // The poster accepts: payable.
        log.push(ev(&poster, SW, 8, 107, review(&sub_a.id, true)));
        let s = State::build(&log);
        assert!(s.payable(&s.submissions[&sub_a.id]));
        let owners = vec![pp.to_string()];
        assert_eq!(s.owed_by(&owners).count(), 1);

        // b and final.
        log.push(ev(&w2, SW, 9, 108, claim(&b)));
        let sub_b = ev(&w2, SW, 10, 109, submit(&b, "B done"));
        log.push(sub_b.clone());
        log.push(ev(&poster, SW, 11, 110, review(&sub_b.id, true)));
        log.push(ev(&w1, SW, 12, 111, claim(&fin)));
        let sub_f = ev(&w1, SW, 13, 112, submit(&fin, "merged"));
        log.push(sub_f.clone());
        // A stranger's reject does not beat the poster's accept.
        log.push(ev(&w2, SW, 14, 113, review(&sub_f.id, false)));
        log.push(ev(&poster, SW, 15, 114, review(&sub_f.id, true)));
        let s = State::build(&log);
        assert!(s.job_complete(&job.id));
        assert_eq!(s.job_result(&job.id).unwrap().content, "merged");

        // Paid marks clear the debt; only an owner may mark.
        log.push(ev(
            &w1,
            SW,
            16,
            115,
            Body::TaskPaid(TaskPaid { submission: sub_a.id.clone(), amount_usd: "0.05".into(), rail_ref: "x".into() }),
        ));
        let s = State::build(&log);
        assert!(s.submissions[&sub_a.id].paid.is_none());
        log.push(ev(
            &poster,
            SW,
            17,
            116,
            Body::TaskPaid(TaskPaid { submission: sub_a.id.clone(), amount_usd: "0.05".into(), rail_ref: "x".into() }),
        ));
        let s = State::build(&log);
        assert!(s.submissions[&sub_a.id].paid.is_some());
    }

    #[test]
    fn order_independent() {
        let (poster, _) = signer();
        let (w1, _) = signer();
        let job = ev(&poster, SW, 1, 1, post(vec![("a", vec![])], ""));
        let a = make_task_id(&job.id, "a");
        let mut log = vec![job, ev(&w1, SW, 2, 2, claim(&a)), ev(&w1, SW, 3, 3, submit(&a, "x"))];
        let one = serde_json::to_string(&State::build(&log).tasks).unwrap();
        log.reverse();
        let two = serde_json::to_string(&State::build(&log).tasks).unwrap();
        assert_eq!(one, two);
    }

    #[test]
    fn lease_expiry_frees_a_task() {
        let (poster, _) = signer();
        let (w1, _) = signer();
        let (w2, p2) = signer();
        let job = ev(&poster, SW, 1, 1, post(vec![("a", vec![])], ""));
        let a = make_task_id(&job.id, "a");
        let log = vec![job.clone(), ev(&w1, SW, 2, 10, claim(&a)), ev(&w2, SW, 3, 100, claim(&a))];
        let s = State::build(&log);
        assert_eq!(s.tasks[&a].claim.as_ref().unwrap().holder, p2.to_string(), "w1's 60 s lease ran out");
    }

    #[test]
    fn unplanned_job_asks_for_a_plan_and_bad_needs_refused() {
        let (poster, _) = signer();
        let (w1, p1) = signer();
        let job = ev(&poster, SW, 1, 1, post(vec![], ""));
        let s = State::build([&job]);
        assert_eq!(s.next_for(&p1.to_string(), 2, |_| true), Next::Plan { job: job.id.clone() });
        // Planner adds tasks; needs must exist.
        let add = |k: &str, needs: Vec<&str>, l| {
            ev(
                &w1,
                SW,
                l,
                2,
                Body::TaskAdd(TaskAdd {
                    job: job.id.clone(),
                    key: k.into(),
                    title: k.into(),
                    desc: String::new(),
                    needs: needs.into_iter().map(String::from).collect(),
                }),
            )
        };
        let s = State::build(&[job.clone(), add("x", vec!["nope"], 2), add("y", vec![], 3), add("z", vec!["y"], 4)]);
        assert!(!s.tasks.contains_key(&make_task_id(&job.id, "x")));
        assert_eq!(s.jobs[&job.id].tasks, vec!["y", "z", FINAL_KEY]);
        let cyc = ev(&poster, SW, 1, 1, post(vec![("a", vec!["b"]), ("b", vec![])], ""));
        assert!(State::build([&cyc]).jobs.is_empty(), "forward needs refused");
    }

    #[test]
    fn close_only_by_owner() {
        let (poster, _) = signer();
        let (w1, _) = signer();
        let job = ev(&poster, SW, 1, 1, post(vec![], ""));
        let close = |k, l| ev(k, SW, l, 2, Body::JobClose(JobRef { job: job.id.clone() }));
        assert!(!State::build(&[job.clone(), close(&w1, 2)]).jobs[&job.id].closed);
        assert!(State::build(&[job.clone(), close(&poster, 2)]).jobs[&job.id].closed);
    }

    #[test]
    fn only_reviewers_with_standing_make_a_result_payable() {
        let paid = |sub: &str, by: &libp2p_kp::Keypair, l| {
            ev(by, SW, l, l, Body::TaskPaid(TaskPaid { submission: sub.into(), amount_usd: "1".into(), rail_ref: String::new() }))
        };
        let (poster, _) = signer();
        let (w, _) = signer();
        let (s1, _) = signer();
        let (s2, _) = signer();
        // Job 1: s1 and s2 each earn standing (paid by the poster).
        let j1 = ev(&poster, SW, 1, 1, post(vec![("a", vec![]), ("b", vec![])], "1"));
        let (a, b) = (make_task_id(&j1.id, "a"), make_task_id(&j1.id, "b"));
        let sa = ev(&s1, SW, 3, 3, submit(&a, "x"));
        let sb = ev(&s2, SW, 5, 5, submit(&b, "y"));
        // Job 2: w's result endorsed by s1 and s2, no owner vote.
        let j2 = ev(&poster, SW, 10, 10, post(vec![("c", vec![])], "1"));
        let c = make_task_id(&j2.id, "c");
        let sc = ev(&w, SW, 12, 12, submit(&c, "z"));
        let mut log = vec![
            j1.clone(), ev(&s1, SW, 2, 2, claim(&a)), sa.clone(), ev(&s2, SW, 4, 4, claim(&b)), sb.clone(),
            j2.clone(), ev(&w, SW, 11, 11, claim(&c)), sc.clone(),
            ev(&s1, SW, 13, 13, review(&sc.id, true)), ev(&s2, SW, 14, 14, review(&sc.id, true)),
        ];
        let s = State::build(&log);
        assert!(s.stands(&s.submissions[&sc.id]), "two endorsements finish the task");
        assert!(!s.payable(&s.submissions[&sc.id]), "fresh identities cannot make it payable");
        // Self-marked payment gives no standing.
        log.push(paid(&sa.id, &s1, 15));
        assert!(!State::build(&log).has_standing(&libp2p_kp::peer(&s1)));
        // Paid by the poster: both reviewers gain standing; now payable.
        log.push(paid(&sa.id, &poster, 16));
        log.push(paid(&sb.id, &poster, 17));
        let s = State::build(&log);
        assert!(s.payable(&s.submissions[&sc.id]));
    }

    mod libp2p_kp {
        pub use c0mpute_net::libp2p::identity::Keypair;
        pub fn peer(k: &Keypair) -> String {
            k.public().to_peer_id().to_string()
        }
    }

    #[test]
    fn unpaid_posters_are_visible() {
        let (poster, pp) = signer();
        let (w1, _) = signer();
        let job = ev(&poster, SW, 1, 1, post(vec![("a", vec![])], "1"));
        let a = make_task_id(&job.id, "a");
        let sub = ev(&w1, SW, 3, 3, submit(&a, "x"));
        let log = vec![job, ev(&w1, SW, 2, 2, claim(&a)), sub.clone(), ev(&poster, SW, 4, 4, review(&sub.id, true))];
        let s = State::build(&log);
        assert_eq!(s.unpaid_count(&pp.to_string(), 10_000, 3600), 1);
        assert_eq!(s.unpaid_count(&pp.to_string(), 100, 3600), 0, "inside the grace period");
    }
}
