# Twiin SMTP — Production Integration & API Documentation

`twiin-smtp` is a resilient, serverless Gmail SMTP microservice backed by Google Cloud Firestore. It provides rate limiting, queuing, automatic retries, and pooled nodemailer connections.

---

## 1. How It Works (Architecture)

```
[ Your Client / App / Backend ]
             │
             ▼ (HTTP POST /api/send)
  ┌───────────────────────┐
  │ 1. Authentication     │ ──> API Key, Firebase ID Token, or Public IP Bucket
  └───────────┬───────────┘
              ▼
  ┌───────────────────────┐
  │ 2. Validation & Quota │ ──> Header injection guard + Firestore daily quota check
  └───────────┬───────────┘
              ▼
  ┌───────────────────────┐
  │ 3. Firestore Queue    │ ──> Atomic job creation (`mail_queue`)
  └───────────┬───────────┘
              ▼
  ┌───────────────────────┐
  │ 4. Nodemailer Worker  │ ──> Pooled connection to Gmail SMTP
  └───────────┬───────────┘
              ├────────────────────────┬────────────────────────┐
              ▼                        ▼                        ▼
        [ Sent (200) ]       [ Rate Limited / Temp ]     [ Fatal Error ]
                                       │                        │
                                       ▼                        ▼
                                Saved as `pending`       Marked `failed`
                                       │
                                       ▼
                             Vercel Cron (/api/cron)
                            (retries with backoff)
```

---

## 2. Authentication Modes

The service supports two authentication modes configured via the `AUTH_MODE` environment variable in `.env`:

### Mode A: Secure Mode (`AUTH_MODE=secure`)
*Recommended for production applications.* All requests must authenticate via one of:
1. **API Key (Backend-to-Backend / Admin):**
   - Header: `x-api-key: YOUR_API_KEY`
   - Bypasses per-user limits (uses only global daily cap).
2. **Firebase ID Token (Frontend / Authenticated Users):**
   - Header: `Authorization: Bearer <Firebase_ID_Token>`
   - Authenticates as a specific user (`uid`). Enforces `USER_DAILY_LIMIT` (default: 20 emails/day).

### Mode B: Open Mode (`AUTH_MODE=open`)
*Ideal for public contact forms or landing pages where visitors don't log in.*
- No headers needed.
- Each visitor's IP is hashed and capped at `PUBLIC_DAILY_LIMIT` (default: 10 emails/day).

---

## 3. API Reference

### 3.1 Send Email
- **Endpoint:** `POST /api/send`
- **Headers:**
  - `Content-Type: application/json`
  - `x-api-key: <API_KEY>` *(if using API key)*
  - `Authorization: Bearer <ID_TOKEN>` *(if using Firebase Auth)*

#### A. Single Email Request Body
```json
{
  "to": "user@example.com",
  "subject": "Welcome to Our Platform!",
  "html": "<h1>Welcome</h1><p>Thank you for signing up.</p>",
  "text": "Welcome! Thank you for signing up.",
  "fromName": "My App Support",
  "replyTo": "support@myapp.com",
  "cc": ["team@myapp.com"],
  "bcc": []
}
```

#### B. Batch Request Body (Up to 100 messages)
```json
{
  "messages": [
    {
      "to": "alice@example.com",
      "subject": "Invoice #101",
      "html": "<p>Your invoice is ready.</p>"
    },
    {
      "to": "bob@example.com",
      "subject": "Invoice #102",
      "html": "<p>Your invoice is ready.</p>"
    }
  ]
}
```

#### Response (200 OK)
```json
{
  "total": 1,
  "sent": 1,
  "failed": 0,
  "rejected": 0,
  "pending": 0,
  "results": [
    {
      "jobId": "abc123xyz",
      "to": ["user@example.com"],
      "status": "sent"
    }
  ]
}
```
*Note on Job Statuses:*
- `sent`: Delivered to Gmail SMTP.
- `pending`: Enqueued for background retry by `/api/cron` (e.g. Gmail temporary rate limit).
- `failed`: Terminal failure (e.g. invalid recipient domain, SMTP permanent reject).
- `rejected`: Request rejected before sending (e.g. validation error or daily quota reached).

---

### 3.2 Check Job Status
- **Endpoint:** `GET /api/status?ids=jobId1,jobId2`
- **Headers:** `x-api-key` or `Authorization: Bearer <token>`
- **Response:**
```json
{
  "jobs": [
    {
      "id": "abc123xyz",
      "status": "sent",
      "attempts": 1,
      "updatedAt": 1718000000000
    }
  ]
}
```

---

### 3.3 Health Check
- **Endpoint:** `GET /api/health`
- **Response:**
```json
{
  "ok": true,
  "service": "twiin-smtp",
  "configured": true
}
```

---

## 4. Production Integration Code Examples

### Example 1: From a Backend Server (Node.js / Express / Next.js API / NestJS)

```javascript
// emailService.js
const SMTP_SERVICE_URL = process.env.SMTP_SERVICE_URL || 'https://your-twiin-smtp.vercel.app';
const SMTP_API_KEY = process.env.SMTP_API_KEY;

/**
 * Sends a transactional email via twiin-smtp microservice.
 */
async function sendEmail({ to, subject, html, text, fromName, replyTo }) {
  const response = await fetch(`${SMTP_SERVICE_URL}/api/send`, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      'x-api-key': SMTP_API_KEY,
    },
    body: JSON.stringify({
      to,
      subject,
      html,
      text,
      fromName,
      replyTo,
    }),
  });

  const data = await response.json();
  if (!response.ok) {
    throw new Error(data.error || `Failed to send email (${response.status})`);
  }

  return data;
}

// Usage:
// await sendEmail({
//   to: 'customer@example.com',
//   subject: 'Password Reset',
//   html: '<p>Click here to reset your password...</p>'
// });
```

---

### Example 2: From a Frontend Client (React / Next.js / Vue / Mobile)

#### With Firebase Authentication:
```javascript
import { getAuth } from 'firebase/auth';

async function sendUserEmail(to, subject, htmlContent) {
  const auth = getAuth();
  const user = auth.currentUser;
  if (!user) throw new Error('User not logged in');

  // Get current user's ID token
  const idToken = await user.getIdToken();

  const res = await fetch('https://your-twiin-smtp.vercel.app/api/send', {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      'Authorization': `Bearer ${idToken}`,
    },
    body: JSON.stringify({
      to,
      subject,
      html: htmlContent,
    }),
  });

  return await res.json();
}
```

#### Without Authentication (Public Contact Form in Open Mode):
```javascript
async function submitContactForm(formData) {
  const res = await fetch('https://your-twiin-smtp.vercel.app/api/send', {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
    },
    body: JSON.stringify({
      to: 'support@yourdomain.com',
      subject: `New Contact Request from ${formData.name}`,
      html: `<p><strong>From:</strong> ${formData.email}</p><p>${formData.message}</p>`,
      replyTo: formData.email,
    }),
  });

  if (!res.ok) {
    const errorData = await res.json();
    throw new Error(errorData.error || 'Failed to submit form');
  }

  return await res.json();
}
```

---

### Example 3: Python (Django / FastAPI / Flask)

```python
import os
import requests

SMTP_URL = os.getenv("SMTP_SERVICE_URL", "https://your-twiin-smtp.vercel.app/api/send")
SMTP_API_KEY = os.getenv("SMTP_API_KEY")

def send_mail(to_email: str, subject: str, html_body: str):
    headers = {
        "Content-Type": "application/json",
        "x-api-key": SMTP_API_KEY
    }
    payload = {
        "to": to_email,
        "subject": subject,
        "html": html_body
    }
    response = requests.post(SMTP_URL, json=payload, headers=headers)
    response.raise_for_status()
    return response.json()
```

---

## 5. Limits & Configuration Reference

Configure these in your `.env` or Vercel Environment Variables:

| Variable | Default | Purpose |
| :--- | :--- | :--- |
| `AUTH_MODE` | `open` | `open` (IP rate limited) or `secure` (API key / Firebase Token required) |
| `API_KEY` | - | Admin secret key for backend calls |
| `DAILY_LIMIT` | `450` | Total global recipients per day (Gmail cap is ~500) |
| `USER_DAILY_LIMIT` | `20` | Max recipients per Firebase authenticated user/day |
| `PUBLIC_DAILY_LIMIT` | `10` | Max recipients per IP/day in open mode |
| `MAX_BATCH` | `100` | Max messages allowed in a single `messages: [...]` request |
| `MAX_RECIPIENTS` | `50` | Max recipients (to + cc + bcc) per single message |
| `ALLOWED_ORIGIN` | `*` | CORS origin (e.g. `https://myapp.com`) |
| `CRON_SECRET` | - | Secret for Vercel Cron trigger |

---

## 6. Checklist for Production Deployment

1. **Deploy to Vercel**:
   ```bash
   vercel --prod
   ```
2. **Add Environment Variables on Vercel Dashboard**:
   - `GMAIL_USER`: Your Gmail address.
   - `GMAIL_APP_PASSWORD`: 16-character Google App Password (2FA enabled).
   - `FIREBASE_PROJECT_ID`: Your Firebase project ID.
   - `FIREBASE_CLIENT_EMAIL`: Service account email.
   - `FIREBASE_PRIVATE_KEY`: Service account private key.
   - `API_KEY`: Strong random secret for your backend services.
   - `AUTH_MODE`: Set to `secure`.
3. **Firestore Setup**:
   - Collections `mail_queue` and `mail_usage` are auto-created.
   - In Firestore Security Rules, lock them from direct client writes:
     ```
     match /mail_queue/{id} { allow read, write: if false; }
     match /mail_usage/{id} { allow read, write: if false; }
     ```
   - In Firestore Console > TTL, set TTL policy on field `expireAt` for both collections to auto-delete aged jobs.

