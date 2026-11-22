'use strict';

// GET /api/health - no auth, reveals nothing but up/configured.

const config = require('../src/config');
const { route } = require('../src/http');

module.exports = route(
  ['GET'],
  async () => ({ ok: true, service: 'twiin-smtp', configured: config.missing().length === 0 }),
  { requireConfig: false }
);
