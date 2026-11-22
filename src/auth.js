'use strict';

const crypto = require('crypto');
const config = require('./config');
const { auth } = require('./firebase');
const { HttpError } = require('./errors');

function safeEqual(a, b) {
  const x = Buffer.from(String(a));
  const y = Buffer.from(String(b));
  return x.length === y.length && crypto.timingSafeEqual(x, y);
}

const bearer = (req) => {
  const m = /^Bearer (.+)$/i.exec(req.headers.authorization || '');
  return m ? m[1] : '';
};

// Hashed client IP, used only as a quota bucket (the raw IP is never stored).
function ipBucket(req) {
  const ip = String(req.headers['x-forwarded-for'] || '').split(',')[0].trim() || (req.socket && req.socket.remoteAddress) || 'unknown';
  return 'ip_' + crypto.createHash('sha256').update(ip + config.firebase.projectId).digest('hex').slice(0, 16);
}

// Identity, in order:
//  - x-api-key header                          -> "admin"  (no per-user cap)
//  - Authorization: Bearer <Firebase ID token> -> "user"
//  - nothing, and AUTH_MODE=open               -> "public" (capped per IP per day)
//  - nothing, and AUTH_MODE=secure             -> 401
async function authenticate(req) {
  const key = req.headers['x-api-key'];
  if (key) {
    if (config.apiKey && safeEqual(key, config.apiKey)) return { role: 'admin', uid: 'admin' };
    throw new HttpError(401, 'Invalid API key');
  }
  const token = bearer(req);
  if (token) {
    try {
      const decoded = await auth().verifyIdToken(token);
      return { role: 'user', uid: decoded.uid };
    } catch {
      throw new HttpError(401, 'Invalid or expired token');
    }
  }
  if (config.authMode === 'open') return { role: 'public', uid: ipBucket(req) };
  throw new HttpError(401, 'Unauthorized');
}

// Vercel Cron sends "Authorization: Bearer <CRON_SECRET>" automatically.
// Never public, even in open mode.
function requireCron(req) {
  const ok =
    (config.cronSecret && safeEqual(bearer(req), config.cronSecret)) ||
    (config.apiKey && req.headers['x-api-key'] && safeEqual(req.headers['x-api-key'], config.apiKey));
  if (!ok) throw new HttpError(401, 'Unauthorized');
}

module.exports = { authenticate, requireCron };
