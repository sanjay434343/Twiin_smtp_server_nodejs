'use strict';

// GET|POST /api/cron  - drains pending/expired-lease jobs.
// Called by Vercel Cron (Bearer CRON_SECRET) or manually with x-api-key.

const { route } = require('../src/http');
const { requireCron } = require('../src/auth');
const { drain } = require('../src/queue');

module.exports = route(['GET', 'POST'], async (req) => {
  requireCron(req);
  return drain();
});
