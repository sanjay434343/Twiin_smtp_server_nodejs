'use strict';

// POST /api/send
// Open mode (default): no key needed. Secure mode: x-api-key or Firebase ID token.
// Body: single message {to, subject, html|text, cc?, bcc?, replyTo?, fromName?}
//       or {messages: [ ...up to MAX_BATCH ]}

const config = require('../src/config');
const { route, parseBody } = require('../src/http');
const { authenticate } = require('../src/auth');
const { normalizeMessage, recipientCount } = require('../src/validate');
const { reserveQuota, enqueue, processJobs } = require('../src/queue');
const { HttpError } = require('../src/errors');

const userLimitFor = (who) =>
  who.role === 'admin' ? null : who.role === 'public' ? config.limits.publicDaily : config.limits.dailyPerUser;

module.exports = route(['POST'], async (req) => {
  const who = await authenticate(req);
  const body = parseBody(req);

  const raw = Array.isArray(body.messages) ? body.messages : [body];
  if (!raw.length) throw new HttpError(400, 'No messages');
  if (raw.length > config.queue.maxBatch) throw new HttpError(413, `Max ${config.queue.maxBatch} messages per request`);

  const results = new Array(raw.length);
  const valid = [];
  raw.forEach((m, i) => {
    const v = normalizeMessage(m);
    if (v.ok) valid.push({ i, msg: v.value });
    else results[i] = { to: m && m.to, status: 'rejected', error: v.error };
  });

  if (valid.length) {
    const recipients = valid.reduce((n, v) => n + recipientCount(v.msg), 0);
    await reserveQuota(who.uid, userLimitFor(who), recipients);
    const jobs = await enqueue(valid.map((v) => v.msg), who.uid);
    const out = await processJobs(jobs);
    valid.forEach((v, k) => (results[v.i] = out[k]));
  }

  const count = (s) => results.filter((r) => r.status === s).length;
  return {
    total: results.length,
    sent: count('sent'),
    failed: count('failed'),
    rejected: count('rejected'),
    pending: count('pending'), // saved in the queue, retried by /api/cron
    results,
  };
});
