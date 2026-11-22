# twiin-smtp

Serverless Gmail SMTP service for Vercel with a Firestore-backed queue.

## Structure
```
api/            HTTP endpoints (one file each, thin)
  send.js       POST   enqueue + deliver (single or batch)
  status.js     GET    job status by id
  cron.js       GET/POST drain retries (Vercel Cron or manual)
  health.js     GET    up / configured
src/            modules
  config.js     env parsing + limits
  firebase.js   Admin SDK init (Firestore, Auth)
  auth.js       API key / Firebase ID token / cron secret
  validate.js   input checks, header-injection guard
  mailer.js     pooled nodemailer transport, error classification
  queue.js      quota, enqueue, worker pool, retry, drain
  http.js       CORS, method guard, uniform errors
  errors.js     HttpError
tools/test.html test UI (open locally)
```

## Request
```
POST /api/send
x-api-key: <API_KEY>              (or Authorization: Bearer <Firebase ID token>)
{"to":"a@b.com","subject":"Hi","html":"<b>hi</b>"}
{"messages":[{...},{...}]}        up to MAX_BATCH
```
Response: `{total,sent,failed,rejected,pending,results:[{jobId,to,status,error?}]}`
Poll: `GET /api/status?ids=<jobId>,<jobId>`

## Firestore setup (once)
- Collections `mail_queue` and `mail_usage` are created automatically.
- Lock them from client SDKs (the server uses the Admin SDK, which bypasses rules). Merge into your existing rules:
  `match /mail_queue/{id} { allow read, write: if false; }`
  `match /mail_usage/{id} { allow read, write: if false; }`
- Console > Firestore > TTL: add a policy on field `expireAt` for both collections (auto-deletes old jobs).

## Deploy
**Open mode (default):** no key/login needed; each IP is capped at `PUBLIC_DAILY_LIMIT` recipients/day and the whole service at `DAILY_LIMIT`. Set `AUTH_MODE=secure` to require an API key or Firebase ID token.

Add every variable from `.env` in Vercel > Settings > Environment Variables, then `vercel --prod`.
