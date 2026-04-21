# Push Notification Testing Guide

## Quick Start

```bash
# Run all tests (unit + mock integration)
cd submodules/runtime
cargo test -p symbiotic-push --features symbiotic-push/test-util

# Run only unit tests (no feature flag needed)
cargo test -p symbiotic-push

# Run only mock integration tests
cargo test -p symbiotic-push --features symbiotic-push/test-util --test push_integration

# Run real APNs/FCM tests (requires credentials)
cargo test -p symbiotic-push --features symbiotic-push/test-util --test push_integration -- --ignored

# Run a specific real test
cargo test -p symbiotic-push --features symbiotic-push/test-util \
  --test push_integration real_apns_sandbox_send -- --ignored
```

## Test Architecture

### Unit Tests (`src/tests.rs`, `src/gateway.rs`)

Standard Rust unit tests covering:
- Token store CRUD operations and validation
- Notification builder and payload factory methods
- JWT generation (APNs ES256, FCM RS256) and caching
- APNs response parsing
- Dispatcher fan-out and auto-pruning
- Type serialization roundtrips
- MockGateway behavior

These run without any feature flags or network access.

### Mock Integration Tests (`tests/push_integration.rs`)

End-to-end tests that spin up real HTTP servers (via `wiremock`) simulating APNs and FCM endpoints. These exercise the full code path:

1. JWT/OAuth2 token generation and signing
2. HTTP request construction with proper headers
3. Response parsing and error handling
4. Dispatcher integration with real gateway instances

**Requires**: `--features symbiotic-push/test-util`

#### APNs Mock Scenarios

| Test | Mock Response | Verifies |
|------|--------------|----------|
| `apns_mock_success_sends_and_records` | 200 OK | Full payload structure, headers, auth |
| `apns_mock_forbidden_returns_failure` | 403 Forbidden | JWT expiry handling |
| `apns_mock_gone_marks_token_invalid` | 410 Gone | Token invalidation flag |
| `apns_mock_rate_limited_returns_error` | 429 Rate Limited | `RateLimited` error + retry-after |
| `apns_mock_bad_request_returns_failure` | 400 Bad Request | Error reason parsing |
| `apns_mock_validates_jwt_auth_header` | Dynamic (200/403) | JWT structure validation |
| `apns_sends_notification_data_fields` | 200 OK | Custom data, thread-id, category |
| `apns_missing_token_returns_error` | N/A | `InvalidToken` error pre-send |

#### FCM Mock Scenarios

| Test | Mock Response | Verifies |
|------|--------------|----------|
| `fcm_mock_success_sends_and_records` | 200 OK | OAuth2 exchange + push payload |
| `fcm_mock_unauthorized_returns_failure` | 401 Unauthorized | Auth error handling |
| `fcm_mock_not_found_marks_token_invalid` | 404 Not Found | Token invalidation |
| `fcm_mock_rate_limited_returns_error` | 429 Rate Limited | `RateLimited` error |
| `fcm_mock_token_exchange_failure` | 400 on token exchange | OAuth2 failure path |
| `fcm_sends_collapse_key_for_thread_id` | 200 OK | Android collapse key mapping |
| `fcm_missing_token_returns_error` | N/A | `InvalidToken` error pre-send |

#### Dispatcher Integration

| Test | Scenario | Verifies |
|------|----------|----------|
| `dispatcher_with_mock_apns_server` | Success path | Full dispatch through mock APNs |
| `dispatcher_auto_prunes_with_mock_apns_gone` | 410 response | Auto-prune removes token from store |

### Real Provider Tests (credential-gated)

Tests marked `#[ignore]` that send actual push notifications to Apple/Google servers. Only run when environment variables are set. Tests use `skip_unless_creds!` for graceful skipping when credentials are absent.

#### Real APNs Tests

| Test | What It Verifies |
|------|-----------------|
| `real_apns_sandbox_send` | Basic sandbox push delivery with title, body, sound |
| `real_apns_production_send` | Production APNs delivery (separate device token) |
| `real_apns_invalid_token` | Gateway handles 410 Gone / 400 BadDeviceToken correctly |
| `real_apns_batch_send` | 3 sequential pushes complete without deadlock or panic |
| `real_apns_rich_payload` | Full payload: badge, sound, thread-id, custom data |
| `real_apns_dispatcher_e2e` | Full dispatcher stack: store -> dispatch -> gateway -> APNs |

#### Real FCM Tests

| Test | What It Verifies |
|------|-----------------|
| `real_fcm_send` | Basic FCM push delivery with OAuth2 auth |
| `real_fcm_invalid_token` | Gateway handles 404 / 400 for invalid registration token |
| `real_fcm_data_message` | Data-only push with custom fields for background processing |
| `real_fcm_topic_send` | Send to FCM topic (requires `FCM_TEST_TOPIC` env var) |

---

## Setting Up Real APNs Testing

### Step 1: Apple Developer Account

1. Sign in to [App Store Connect](https://appstoreconnect.apple.com/)
2. Navigate to **Users and Access** > **Integrations** > **Keys**
3. Or use the [Apple Developer Portal](https://developer.apple.com/account/resources/authkeys/list)

### Step 2: Create an APNs Authentication Key

1. In the Apple Developer Portal, go to **Certificates, Identifiers & Profiles** > **Keys**
2. Click the **+** button to create a new key
3. Enter a name (e.g., "Symbiotic Push Key")
4. Check **Apple Push Notifications service (APNs)**
5. Click **Continue**, then **Register**
6. **Download the `.p8` file** immediately (Apple only lets you download it once)
7. Note the **Key ID** shown on the confirmation page (10-character string, e.g., `ABC123DEFG`)
8. Note your **Team ID** from the top-right of the developer portal (10-character string, e.g., `9YK6WX7ABC`)

### Step 3: Get a Device Token

1. Build the Symbiotic app in **development** mode on a **physical iOS device** (simulators cannot receive real push notifications)
2. The device token is logged in `AppDelegate.swift` during `application:didRegisterForRemoteNotificationsWithDeviceToken:`
3. The token is a 64-character hex string, e.g., `a1b2c3d4e5f6...` (32 bytes in hex)
4. If using the Flutter app, the `PushService` logs the token via the platform channel

### Step 4: Set Environment Variables

```bash
# Required for all APNs tests
export APNS_TEST_TEAM_ID="YOUR_TEAM_ID"          # 10-char Team ID
export APNS_TEST_KEY_ID="YOUR_KEY_ID"             # 10-char Key ID from step 2
export APNS_TEST_KEY="$(cat path/to/AuthKey_KEYID.p8)"  # Raw .p8 key content

# Required for sandbox tests (development builds)
export APNS_TEST_DEVICE_TOKEN="64_char_hex_token"

# Optional: for production tests (App Store / TestFlight builds)
export APNS_TEST_PRODUCTION_DEVICE_TOKEN="64_char_hex_production_token"
```

### Step 5: Run Tests

```bash
# Single sandbox test
cargo test -p symbiotic-push --features symbiotic-push/test-util \
  --test push_integration real_apns_sandbox_send -- --ignored

# All APNs real tests
cargo test -p symbiotic-push --features symbiotic-push/test-util \
  --test push_integration real_apns -- --ignored

# Invalid token test (only needs credentials, not a real device token)
cargo test -p symbiotic-push --features symbiotic-push/test-util \
  --test push_integration real_apns_invalid_token -- --ignored
```

The sandbox tests send to `api.sandbox.push.apple.com`. The push may or may not arrive on the device depending on token validity, but the tests verify the gateway handles responses correctly in all cases.

---

## Setting Up Real FCM Testing

### Step 1: Firebase Project Setup

1. Go to the [Firebase Console](https://console.firebase.google.com/)
2. Create a new project or select an existing one
3. Navigate to **Project Settings** (gear icon) > **Cloud Messaging**
4. Ensure the **Cloud Messaging API (V1)** is enabled (it should be by default for new projects)

### Step 2: Create a Service Account Key

1. In Firebase Console, go to **Project Settings** > **Service accounts**
2. Click **Generate new private key**
3. Download the JSON key file (keep this secure — it grants API access)
4. The JSON file contains `project_id`, `client_email`, and `private_key` fields

### Step 3: Get an FCM Registration Token

1. Build the Symbiotic app with Firebase configured on an Android device or emulator
2. In Flutter, call `FirebaseMessaging.instance.getToken()` to get the registration token
3. The token looks like `fMh7yK3dT9q:APA91bH_abc123...` (variable length string)
4. For testing without the app, you can use the Firebase Admin SDK to generate a test token

### Step 4: Set Environment Variables

```bash
# Required for all FCM tests — the full JSON content of the service account key
export FCM_TEST_CREDENTIALS='{"type":"service_account","project_id":"your-project","client_email":"firebase-adminsdk@your-project.iam.gserviceaccount.com","private_key":"-----BEGIN RSA PRIVATE KEY-----\nMIIEv...\n-----END RSA PRIVATE KEY-----\n"}'

# Required for send tests
export FCM_TEST_DEVICE_TOKEN="your_fcm_registration_token"

# Optional: for topic tests
export FCM_TEST_TOPIC="test-notifications"
```

**Tip**: For the credentials JSON, you can read the file directly:
```bash
export FCM_TEST_CREDENTIALS="$(cat path/to/service-account.json)"
```

### Step 5: Run Tests

```bash
# Single FCM test
cargo test -p symbiotic-push --features symbiotic-push/test-util \
  --test push_integration real_fcm_send -- --ignored

# All FCM real tests
cargo test -p symbiotic-push --features symbiotic-push/test-util \
  --test push_integration real_fcm -- --ignored

# Invalid token test (only needs credentials, not a real device token)
cargo test -p symbiotic-push --features symbiotic-push/test-util \
  --test push_integration real_fcm_invalid_token -- --ignored
```

---

## Test Utilities

### `skip_unless_creds!` Macro

Gracefully skip tests when credentials are absent instead of panicking:

```rust
use symbiotic_push::skip_unless_creds;

#[tokio::test]
#[ignore]
async fn my_real_test() {
    skip_unless_creds!("APNS_TEST_KEY", "APNS_TEST_TEAM_ID");
    // Test body — only runs if all env vars are set
}
```

### Config Builders

```rust
use symbiotic_push::testutil::{setup_real_apns_config, setup_real_fcm_config};

// Build APNs config from env vars (panics if missing — use skip_unless_creds! first)
let apns_config = setup_real_apns_config(true);  // true = sandbox

// Build FCM config from env vars
let fcm_config = setup_real_fcm_config();
```

### Token Readers

```rust
use symbiotic_push::testutil::{real_apns_device_token, real_fcm_device_token};

let apns_token = real_apns_device_token();  // reads APNS_TEST_DEVICE_TOKEN
let fcm_token = real_fcm_device_token();    // reads FCM_TEST_DEVICE_TOKEN
```

### Delivery Assertion

```rust
use symbiotic_push::testutil::assert_push_accepted;

let response = gateway.send(&notification).await?;
assert_push_accepted(&response);  // asserts success=true and provider_message_id.is_some()
```

---

## CI/CD Configuration

### Default CI Pipeline

```yaml
- name: Push notification tests (mock)
  run: |
    cd submodules/runtime
    cargo test -p symbiotic-push --features symbiotic-push/test-util
```

This runs all unit tests and mock integration tests. No credentials needed.

### Optional Real Provider Tests (GitHub Actions)

```yaml
- name: Real APNs tests
  if: env.APNS_TEST_KEY != ''
  env:
    APNS_TEST_TEAM_ID: ${{ secrets.APNS_TEST_TEAM_ID }}
    APNS_TEST_KEY_ID: ${{ secrets.APNS_TEST_KEY_ID }}
    APNS_TEST_KEY: ${{ secrets.APNS_TEST_KEY }}
    APNS_TEST_DEVICE_TOKEN: ${{ secrets.APNS_TEST_DEVICE_TOKEN }}
  run: |
    cd submodules/runtime
    cargo test -p symbiotic-push --features symbiotic-push/test-util \
      --test push_integration real_apns -- --ignored

- name: Real FCM tests
  if: env.FCM_TEST_CREDENTIALS != ''
  env:
    FCM_TEST_CREDENTIALS: ${{ secrets.FCM_TEST_CREDENTIALS }}
    FCM_TEST_DEVICE_TOKEN: ${{ secrets.FCM_TEST_DEVICE_TOKEN }}
  run: |
    cd submodules/runtime
    cargo test -p symbiotic-push --features symbiotic-push/test-util \
      --test push_integration real_fcm -- --ignored
```

### Setting Up GitHub Actions Secrets

1. Go to your repository **Settings** > **Secrets and variables** > **Actions**
2. Add each secret:
   - `APNS_TEST_TEAM_ID` — Your 10-character Apple Team ID
   - `APNS_TEST_KEY_ID` — Your 10-character APNs Key ID
   - `APNS_TEST_KEY` — The full content of the `.p8` file (including `-----BEGIN PRIVATE KEY-----` header/footer)
   - `APNS_TEST_DEVICE_TOKEN` — 64-character hex device token from a dev build
   - `FCM_TEST_CREDENTIALS` — The full JSON content of the Firebase service account key file
   - `FCM_TEST_DEVICE_TOKEN` — FCM registration token from a device

**Security notes**:
- GitHub Actions secrets are encrypted at rest and masked in logs
- The `.p8` key and service account JSON grant API access — treat them like passwords
- Device tokens change when apps are reinstalled; update secrets periodically
- Consider using a dedicated test device / Firebase project for CI

---

## Using Test Utilities in Other Crates

The `test-util` feature exposes `symbiotic_push::testutil` with:

- `mock_apns::start_mock_apns_success()` (and other scenarios)
- `mock_fcm::start_mock_fcm_success()` (and other scenarios)
- `mock_apns_config(url)` / `mock_fcm_config(url)` — pre-configured gateway configs
- `setup_real_apns_config(sandbox)` / `setup_real_fcm_config()` — real credential configs
- `real_apns_device_token()` / `real_fcm_device_token()` — env var readers
- `fake_apns_device_token()` / `fake_fcm_registration_token()` — test tokens
- `generate_es256_test_key()` / `generate_rsa_test_key()` — cryptographic key generators
- `assert_push_accepted(response)` — delivery acceptance assertion
- `skip_unless_creds!()` — macro for graceful credential gating

Add to your crate's `Cargo.toml`:

```toml
[dev-dependencies]
symbiotic-push = { path = "../symbiotic-push", features = ["test-util"] }
```

---

## Troubleshooting

### APNs returns 403 Forbidden / ExpiredProviderToken

- The `.p8` key may have been revoked in App Store Connect
- The Team ID or Key ID may be incorrect
- The key may not have APNs capability enabled

### APNs returns 400 BadDeviceToken

- The device token is malformed (not 64 hex characters)
- The token was generated for a different APNs environment (sandbox vs production)
- The token was generated for a different app bundle ID

### APNs returns 410 Gone / Unregistered

- The device token is no longer valid (app uninstalled, token rotated)
- Get a fresh token by rebuilding and relaunching the app

### FCM returns 401 Unauthorized

- The service account JSON may be invalid or for the wrong project
- The Cloud Messaging API may not be enabled for the project
- The private key in the JSON may have been rotated

### FCM returns 404 Not Found

- The registration token is invalid or expired
- Get a fresh token from the app

### Tests hang or timeout

- Check network connectivity (APNs uses port 443 with HTTP/2)
- If behind a corporate proxy, APNs/FCM endpoints may be blocked
- The `reqwest` client may need proxy configuration
