/**
 * Mock login server for testing auth scripts.
 *
 * Serves a simple login form at /login with username + password fields.
 * Validates hardcoded credentials (testuser / testpass123).
 * On success: sets session_token cookie and redirects to /dashboard.
 *
 * Usage: npx ts-node server.ts [port]
 * Default port: 3847
 */

import * as http from 'http';
import * as url from 'url';
import * as querystring from 'querystring';

const VALID_USERNAME = 'testuser';
const VALID_PASSWORD = 'testpass123';
const SESSION_TOKEN = 'abc123';

const port = parseInt(process.argv[2] || '3847', 10);

const LOGIN_HTML = `<!DOCTYPE html>
<html>
<head><title>Mock Login</title></head>
<body>
  <h1>Login</h1>
  <form method="POST" action="/login">
    <label for="username">Username</label>
    <input type="text" id="username" name="username" autocomplete="username" />
    <label for="password">Password</label>
    <input type="password" id="password" name="password" autocomplete="current-password" />
    <button type="submit">Sign in</button>
  </form>
</body>
</html>`;

const LOGIN_ERROR_HTML = `<!DOCTYPE html>
<html>
<head><title>Mock Login</title></head>
<body>
  <h1>Login</h1>
  <div class="error" role="alert">Invalid username or password</div>
  <form method="POST" action="/login">
    <label for="username">Username</label>
    <input type="text" id="username" name="username" autocomplete="username" />
    <label for="password">Password</label>
    <input type="password" id="password" name="password" autocomplete="current-password" />
    <button type="submit">Sign in</button>
  </form>
</body>
</html>`;

const DASHBOARD_HTML = `<!DOCTYPE html>
<html>
<head><title>Dashboard</title></head>
<body>
  <h1>Welcome testuser</h1>
  <p>You are logged in.</p>
</body>
</html>`;

function parseBody(req: http.IncomingMessage): Promise<string> {
  return new Promise((resolve, reject) => {
    let body = '';
    req.on('data', (chunk: Buffer) => (body += chunk.toString()));
    req.on('end', () => resolve(body));
    req.on('error', reject);
  });
}

const server = http.createServer(async (req, res) => {
  const parsed = url.parse(req.url || '/', true);
  const pathname = parsed.pathname || '/';
  const method = req.method || 'GET';

  if (pathname === '/login' && method === 'GET') {
    res.writeHead(200, { 'Content-Type': 'text/html' });
    res.end(LOGIN_HTML);
    return;
  }

  if (pathname === '/login' && method === 'POST') {
    const body = await parseBody(req);
    const params = querystring.parse(body);
    const username = params.username as string;
    const password = params.password as string;

    if (username === VALID_USERNAME && password === VALID_PASSWORD) {
      res.writeHead(302, {
        'Set-Cookie': `session_token=${SESSION_TOKEN}; Path=/; HttpOnly`,
        Location: '/dashboard',
      });
      res.end();
    } else {
      res.writeHead(200, { 'Content-Type': 'text/html' });
      res.end(LOGIN_ERROR_HTML);
    }
    return;
  }

  if (pathname === '/dashboard' && method === 'GET') {
    // Check for session cookie
    const cookies = req.headers.cookie || '';
    if (cookies.includes(`session_token=${SESSION_TOKEN}`)) {
      res.writeHead(200, { 'Content-Type': 'text/html' });
      res.end(DASHBOARD_HTML);
    } else {
      res.writeHead(302, { Location: '/login' });
      res.end();
    }
    return;
  }

  // Default: redirect to login
  res.writeHead(302, { Location: '/login' });
  res.end();
});

server.listen(port, '127.0.0.1', () => {
  // Print to stdout so the parent process knows the server is ready
  console.log(`READY:${port}`);
});
