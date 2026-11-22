'use strict';

// GET /api/status?ids=jobId1,jobId2   (max 50)
// Users only see their own jobs; the API key sees all.

const { route } = require('../src/http');
const { authenticate } = require('../src/auth');
const { getStatuses } = require('../src/queue');

module.exports = route(['GET'], async (req) => {
  const who = await authenticate(req);
  const ids = String((req.query && req.query.ids) || '').split(',').map((s) => s.trim()).filter(Boolean);
  return { jobs: await getStatuses(ids, who) };
});
