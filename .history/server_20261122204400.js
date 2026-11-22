'use strict';

// Load .env automatically (supported natively in Node.js 20+)
try {
  if (process.loadEnvFile) {
    process.loadEnvFile();
  }
} catch {
  // .env may not exist yet or is empty
}

const http = require('http');
const fs = require('fs');
const path = require('path');
const url = require('url');

const routes = {
  '/api/health': require('./api/health'),
  '/api/send': require('./api/send'),
  '/api/status': require('./api/status'),
  '/api/cron': require('./api/cron'),
};

const server = http.createServer((req, res) => {
  const parsedUrl = url.parse(req.url, true);
  const pathname = parsedUrl.pathname;
  req.query = parsedUrl.query || {};

  // Augment res with standard helper methods
  res.status = function (code) {
    this.statusCode = code;
    return this;
  };
  res.json = function (data) {
    this.setHeader('Content-Type', 'application/json');
    this.end(JSON.stringify(data));
    return this;
  };

  // Serve test UI on GET root or /tools/test.html
  if (req.method === 'GET' && (pathname === '/' || pathname === '/tools/test.html' || pathname === '/test')) {
    const htmlPath = path.join(__dirname, 'tools', 'test.html');
    if (fs.existsSync(htmlPath)) {
      res.writeHead(200, { 'Content-Type': 'text/html' });
      return fs.createReadStream(htmlPath).pipe(res);
    }
  }

  // Alias POST / or /send to /api/send for convenience
  let targetPath = pathname;
  if (req.method === 'POST' && (pathname === '/' || pathname === '/send')) {
    targetPath = '/api/send';
  }

  // Handle serverless API routes
  const handler = routes[targetPath];
  if (handler) {
    let body = '';
    req.on('data', (chunk) => {
      body += chunk;
    });
    req.on('end', async () => {
      req.body = body;
      try {
        await handler(req, res);
      } catch (err) {
        console.error('Unhandled server error:', err);
        if (!res.headersSent) {
          res.status(500).json({ error: 'Internal server error' });
        }
      }
    });
    return;
  }

  res.status(404).json({ error: 'Not Found' });
});

const PORT = process.env.PORT || 3000;
server.listen(PORT, () => {
  console.log(`> twiin-smtp running at http://localhost:${PORT}`);
  console.log(`> Health check: http://localhost:${PORT}/api/health`);
  console.log(`> Test UI:      http://localhost:${PORT}/`);
});

