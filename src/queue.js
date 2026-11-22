'use strict';

// Firestore-backed job queue.
// Job states: processing (leased by a worker) -> sent | failed | pending (retry later).
// Delivery is at-least-once: if a function dies after Gmail accepted a mail but before
// the status write, the lease expires and the job is retried (rare duplicate possible).

const config = require('./config');
const { db, FieldValue } = require('./firebase');
const mailer = require('./mailer');
const { HttpError } = require('./errors');

const Q = config.queue;
const col = () => db().collection(Q.collection);
const usage = () => db().collection(Q.usageCollection);
const dayKey = () => new Date().toISOString().slice(0, 10); // UTC day
const expiry = (days = Q.retentionDays) => new Date(Date.now() + days * 86400000); // for a Firestore TTL policy on "expireAt"
const backoff = (attempts) => Math.min(60000 * 2 ** (attempts - 1), 30 * 60000);

// ---- bounded worker pool -------------------------------------------------
const SKIPPED = Symbol('skipped');

async function runPool(items, concurrency, deadline, fn) {
  const results = new Array(items.length).fill(SKIPPED);
  let next = 0;
  async function worker() {
    while (true) {
      const i = next++;
      if (i >= items.length || Date.now() > deadline) return;
      results[i] = await fn(items[i]);
    }
  }
  await Promise.all(Array.from({ length: Math.min(concurrency, items.length) }, worker));
  return results;
}

// ---- daily quota ---------------------------------------------------------
// Counts recipients (that is what Gmail limits), globally and per signed-in user.
async function reserveQuota(uid, userLimit, count) {
  const day = dayKey();
  const gRef = usage().doc(`global_${day}`);
  const uRef = userLimit == null ? null : usage().doc(`u_${uid}_${day}`);
  await db().runTransaction(async (tx) => {
    const [g, u] = await Promise.all([tx.get(gRef), uRef ? tx.get(uRef) : Promise.resolve(null)]);
    const gUsed = g.exists ? g.data().count : 0;
    const uUsed = u && u.exists ? u.data().count : 0;
    if (gUsed + count > config.limits.dailyGlobal) {
      throw new HttpError(429, 'Daily sending limit reached', { remaining: Math.max(0, config.limits.dailyGlobal - gUsed) });
    }
    if (uRef && uUsed + count > userLimit) {
      throw new HttpError(429, 'Daily email limit reached for you', { remaining: Math.max(0, userLimit - uUsed) });
    }
    const stamp = { updatedAt: Date.now(), expireAt: expiry(3) };
    tx.set(gRef, { count: gUsed + count, ...stamp }, { merge: true });
    if (uRef) tx.set(uRef, { count: uUsed + count, ...stamp }, { merge: true });
  });
}

// ---- enqueue -------------------------------------------------------------
// Jobs created for immediate processing start leased ("processing") so the cron
// drain does not grab them while this request is working on them.
async function enqueue(messages, uid) {
  const now = Date.now();
  const batch = db().batch();
  const jobs = messages.map((m) => {
    const ref = col().doc();
    const data = { ...m, ownerUid: uid, status: 'processing', attempts: 0, createdAt: now, nextAttemptAt: now, leaseUntil: now + Q.leaseMs };
    batch.set(ref, data);
    return { id: ref.id, ...data };
  });
  await batch.commit();
  return jobs;
}

// ---- settle one job after a send attempt ---------------------------------
async function settle(job, out) {
  const ref = col().doc(job.id);
  const attempts = (job.attempts || 0) + 1;
  const now = Date.now();
  try {
    if (out.ok) {
      await ref.update({
        status: 'sent', attempts, sentAt: now, messageId: out.messageId || null,
        error: FieldValue.delete(), html: FieldValue.delete(), text: FieldValue.delete(),
        leaseUntil: FieldValue.delete(), expireAt: expiry(),
      });
      return { status: 'sent' };
    }
    if (out.permanent || attempts >= Q.maxAttempts) {
      await ref.update({ status: 'failed', attempts, error: out.error, leaseUntil: FieldValue.delete(), expireAt: expiry() });
      return { status: 'failed', error: out.error };
    }
    await ref.update({ status: 'pending', attempts, error: out.error, nextAttemptAt: now + backoff(attempts), leaseUntil: FieldValue.delete() });
    return { status: 'pending', error: out.error };
  } catch (e) {
    console.error('settle failed', job.id, e.message);
    return { status: out.ok ? 'sent' : 'failed', error: 'status write failed' };
  }
}

async function release(jobs) {
  const batch = db().batch();
  jobs.forEach((j) => batch.update(col().doc(j.id), { status: 'pending', leaseUntil: FieldValue.delete() }));
  await batch.commit();
}

// ---- process leased jobs inside a time budget ----------------------------
async function processJobs(jobs) {
  const deadline = Date.now() + Q.timeBudgetMs;
  const results = await runPool(jobs, Q.concurrency, deadline, async (job) => settle(job, await mailer.send(job)));
  const left = jobs.filter((_, i) => results[i] === SKIPPED);
  if (left.length) await release(left);
  return jobs.map((j, i) => ({
    jobId: j.id,
    to: j.to,
    ...(results[i] === SKIPPED ? { status: 'pending' } : results[i]),
  }));
}

// ---- claim due jobs (cron / retry path) ----------------------------------
async function claimDue(limit) {
  const snap = await col().where('status', 'in', ['pending', 'processing']).limit(limit * 3).get();
  const now = Date.now();
  const due = snap.docs
    .filter((d) => {
      const x = d.data();
      return x.status === 'pending' ? (x.nextAttemptAt || 0) <= now : (x.leaseUntil || 0) < now;
    })
    .slice(0, limit);

  const claimed = await Promise.all(
    due.map((d) =>
      db().runTransaction(async (tx) => {
        const s = await tx.get(d.ref);
        const y = s.data();
        const t = Date.now();
        const ok = y && (y.status === 'pending' ? (y.nextAttemptAt || 0) <= t : y.status === 'processing' && (y.leaseUntil || 0) < t);
        if (!ok) return null;
        tx.update(d.ref, { status: 'processing', leaseUntil: t + Q.leaseMs });
        return { id: d.id, ...y, status: 'processing' };
      })
    )
  );
  return claimed.filter(Boolean);
}

async function drain() {
  const jobs = await claimDue(Q.maxBatch);
  const results = jobs.length ? await processJobs(jobs) : [];
  const count = (s) => results.filter((r) => r.status === s).length;
  return { claimed: jobs.length, sent: count('sent'), failed: count('failed'), pending: count('pending') };
}

// ---- status lookup -------------------------------------------------------
async function getStatuses(ids, who) {
  const unique = [...new Set(ids)].slice(0, 50);
  if (!unique.length) return [];
  const snaps = await db().getAll(...unique.map((id) => col().doc(id)));
  return snaps.map((s) => {
    const d = s.exists ? s.data() : null;
    if (!d || (who.role === 'user' && d.ownerUid !== who.uid)) return { jobId: s.id, status: 'not_found' };
    // job ids are unguessable; public callers never see recipient addresses
    return { jobId: s.id, ...(who.role === 'public' ? {} : { to: d.to }), status: d.status, attempts: d.attempts, error: d.error, sentAt: d.sentAt };
  });
}

module.exports = { reserveQuota, enqueue, processJobs, drain, getStatuses };
