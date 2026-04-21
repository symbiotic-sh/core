# Auth Script Registry

Scripts in this directory handle automated authentication for specific domains.

## Naming Convention

- `{domain}.ts` — TypeScript/Playwright script for browser-based login
- `{domain}.sh` — Shell script for API key authentication
- `_generic.ts` — Generic fallback for standard username/password forms

## Protocol

Scripts receive credentials via **stdin** as JSON:
```json
{"domain": "github.com", "username": "user", "password": "...", "totp_code": "123456"}
```

Scripts must output a result via **stdout** as JSON:
```json
{"success": true, "session": "token_or_cookie_here"}
```

Or on failure:
```json
{"success": false, "error": "Invalid credentials"}
```

## Security

- Scripts **never** receive TOTP secrets — only pre-generated 6-digit codes
- Credentials are passed via stdin only, never environment variables
- The subprocess environment is cleared (`env_clear()`)
- A timeout (default 60s) kills scripts that hang
