'use strict';

const env = process.env;

const num = (v, d) => {
  if (v === undefined || v === '') return d;
  const n = Number(v);
  return Number.isFinite(n) ? n : d;
};
const clamp = (n, lo, hi) => Math.max(lo, Math.min(hi, n));
const unquote = (s) => String(s || '').replace(/^"|"$/g, '');

const config = {
  gmail: {
    user: env.GMAIL_USER || '',
    pass: (env.GMAIL_APP_PASSWORD || '').replace(/\s+/g, ''),
  },
  // open = anyone with the URL can send (capped per IP + globally); secure = API key / Firebase token required
  authMode: env.AUTH_MODE === 'secure' ? 'secure' : 'open',
  apiKey: env.API_KEY || '',
  cronSecret: env.CRON_SECRET || '',
  allowedOrigin: env.ALLOWED_ORIGIN || '*',
  firebase: {
    projectId: env.FIREBASE_PROJECT_ID || '',
    clientEmail: env.FIREBASE_CLIENT_EMAIL || '',
    privateKey: unquote(env.FIREBASE_PRIVATE_KEY).replace(/\\n/g, '\n'),
  },
  queue: {
    collection: env.QUEUE_COLLECTION || 'mail_queue',
    usageCollection: env.USAGE_COLLECTION || 'mail_usage',
    concurrency: clamp(num(env.CONCURRENCY, 3), 1, 5),
    maxBatch: clamp(num(env.MAX_BATCH, 100), 1, 400),
    timeBudgetMs: num(env.TIME_BUDGET_MS, 45000),
    leaseMs: num(env.LEASE_MS, 90000),
    maxAttempts: num(env.MAX_ATTEMPTS, 4),
    retentionDays: num(env.RETENTION_DAYS, 7),
  },
  limits: {
    dailyGlobal: num(env.DAILY_LIMIT, 450), // Gmail personal cap is ~500 recipients/day
    dailyPerUser: num(env.USER_DAILY_LIMIT, 20),
    publicDaily: num(env.PUBLIC_DAILY_LIMIT, 10), // recipients per IP per day in open mode
    maxBodyChars: num(env.MAX_BODY_CHARS, 500000), // Firestore doc cap is 1 MiB
    maxRecipients: num(env.MAX_RECIPIENTS, 50),
  },
};

// Names of required env vars that are empty. Checked per request, not at import,
// so a bad deploy returns a clean 500 instead of crashing the function.
function missing() {
  const req = {
    GMAIL_USER: config.gmail.user,
    GMAIL_APP_PASSWORD: config.gmail.pass,
    FIREBASE_PROJECT_ID: config.firebase.projectId,
    FIREBASE_CLIENT_EMAIL: config.firebase.clientEmail,
    FIREBASE_PRIVATE_KEY: config.firebase.privateKey,
  };
  const out = Object.keys(req).filter((k) => !req[k]);
  if (!config.apiKey && !config.cronSecret) out.push('API_KEY or CRON_SECRET');
  return out;
}

module.exports = Object.assign(config, { missing });
