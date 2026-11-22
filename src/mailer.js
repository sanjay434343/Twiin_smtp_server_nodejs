'use strict';

const nodemailer = require('nodemailer');
const config = require('./config');

let transporter;

// One pooled Gmail connection set per warm instance (small, reused across invocations).
function getTransporter() {
  if (!transporter) {
    transporter = nodemailer.createTransport({
      host: 'smtp.gmail.com',
      port: 465,
      secure: true,
      pool: true,
      maxConnections: config.queue.concurrency,
      maxMessages: 100,
      auth: { user: config.gmail.user, pass: config.gmail.pass },
    });
  }
  return transporter;
}

// permanent = retrying will not help (bad address, bad credentials)
// transient = try again later (network, 4xx, Gmail rate/quota limits)
function isPermanent(err) {
  if (/daily user sending|quota|rate limit|too many/i.test(err.message || '')) return false;
  if (err.code === 'EAUTH' || err.code === 'EENVELOPE') return true;
  return err.responseCode >= 500;
}

async function send(job) {
  try {
    const info = await getTransporter().sendMail({
      from: job.fromName ? `"${job.fromName}" <${config.gmail.user}>` : config.gmail.user,
      to: job.to,
      cc: job.cc && job.cc.length ? job.cc : undefined,
      bcc: job.bcc && job.bcc.length ? job.bcc : undefined,
      replyTo: job.replyTo,
      subject: job.subject,
      html: job.html || undefined,
      text: job.text || undefined,
    });
    return { ok: true, messageId: info.messageId };
  } catch (err) {
    return { ok: false, permanent: isPermanent(err), error: String(err.message || err).slice(0, 300) };
  }
}

module.exports = { send };
