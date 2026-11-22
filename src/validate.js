'use strict';

const config = require('./config');

const EMAIL_RE = /^[^\s@<>"',;]+@[^\s@<>"',;]+\.[^\s@<>"',;]+$/;

const toList = (v) =>
  [...new Set((Array.isArray(v) ? v : v ? [v] : []).map((s) => String(s).trim().toLowerCase()).filter(Boolean))];
const oneLine = (s) => String(s).replace(/[\r\n]+/g, ' ').trim(); // blocks header injection

const fail = (error) => ({ ok: false, error });

function normalizeMessage(m) {
  if (!m || typeof m !== 'object') return fail('Message must be an object');

  const to = toList(m.to);
  const cc = toList(m.cc);
  const bcc = toList(m.bcc);
  if (!to.length) return fail('"to" is required');

  const all = [...to, ...cc, ...bcc];
  if (all.length > config.limits.maxRecipients) return fail(`Max ${config.limits.maxRecipients} recipients per message`);
  const bad = all.find((e) => e.length > 254 || !EMAIL_RE.test(e));
  if (bad) return fail(`Invalid email: ${bad}`);

  const subject = oneLine(m.subject || '');
  if (!subject) return fail('"subject" is required');
  if (subject.length > 200) return fail('Subject too long (max 200)');

  const html = typeof m.html === 'string' ? m.html : '';
  const text = typeof m.text === 'string' ? m.text : '';
  if (!html && !text) return fail('"html" or "text" is required');
  if (html.length + text.length > config.limits.maxBodyChars) return fail('Body too large');

  let replyTo;
  if (m.replyTo) {
    replyTo = String(m.replyTo).trim();
    if (!EMAIL_RE.test(replyTo)) return fail('Invalid replyTo');
  }
  const fromName = m.fromName ? oneLine(m.fromName).replace(/["<>]/g, '').slice(0, 80) : '';

  return { ok: true, value: { to, cc, bcc, subject, html, text, replyTo, fromName } };
}

const recipientCount = (msg) => msg.to.length + msg.cc.length + msg.bcc.length;

module.exports = { normalizeMessage, recipientCount };
