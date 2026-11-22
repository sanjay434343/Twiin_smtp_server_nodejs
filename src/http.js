'use strict';

const config = require('./config');
const { HttpError } = require('./errors');

function setCors(res) {
  res.setHeader('Access-Control-Allow-Origin', config.allowedOrigin);
  res.setHeader('Access-Control-Allow-Methods', 'GET, POST, OPTIONS');
  res.setHeader('Access-Control-Allow-Headers', 'Content-Type, Authorization, x-api-key');
}

function parseBody(req) {
  let b = req.body;
  if (typeof b === 'string') {
    try {
      b = JSON.parse(b);
    } catch {
      throw new HttpError(400, 'Invalid JSON');
    }
  }
  if (!b || typeof b !== 'object' || Array.isArray(b)) throw new HttpError(400, 'JSON object body required');
  return b;
}

// Wraps a handler: CORS, method guard, env check, uniform error output.
// The handler returns a JSON-serialisable object (sent as 200) or throws HttpError.
function route(methods, fn, opts = {}) {
  const allowed = new Set(methods);
  return async (req, res) => {
    setCors(res);
    if (req.method === 'OPTIONS') return res.status(204).end();
    if (!allowed.has(req.method)) return res.status(405).json({ error: 'Method not allowed' });
    try {
      if (opts.requireConfig !== false) {
        const miss = config.missing();
        if (miss.length) {
          console.error('Missing env:', miss.join(', '));
          throw new HttpError(500, 'Server not configured');
        }
      }
      const out = await fn(req, res);
      if (!res.headersSent) res.status(200).json(out);
    } catch (e) {
      if (e instanceof HttpError) return res.status(e.status).json({ error: e.message, ...e.extra });
      console.error(e);
      return res.status(500).json({ error: 'Internal error' });
    }
  };
}

module.exports = { route, parseBody };
