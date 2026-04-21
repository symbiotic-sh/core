//! TOTP (RFC 6238) and HOTP (RFC 4226) implementation.
//!
//! Generates time-based one-time passwords from shared secrets,
//! used by the Auth Script Engine for automated 2FA handling.

use hmac::{Hmac, Mac};
use sha1::Sha1;
use thiserror::Error;

type HmacSha1 = Hmac<Sha1>;

#[derive(Debug, Error)]
pub enum TotpError {
    #[error("invalid base32 secret")]
    InvalidBase32,
    #[error("empty secret")]
    EmptySecret,
    #[error("HMAC computation failed")]
    HmacError,
}

/// TOTP parameters (defaults: step=30s, digits=6, t0=0)
pub struct TotpParams {
    pub step: u64,
    pub digits: u32,
    pub t0: u64,
}

impl Default for TotpParams {
    fn default() -> Self {
        Self {
            step: 30,
            digits: 6,
            t0: 0,
        }
    }
}

/// Generate a TOTP code from a base32-encoded secret and current time.
pub fn generate_totp(
    secret_base32: &str,
    time_secs: u64,
    params: &TotpParams,
) -> Result<String, TotpError> {
    if secret_base32.is_empty() {
        return Err(TotpError::EmptySecret);
    }
    let secret = decode_base32(secret_base32)?;
    let counter = (time_secs.saturating_sub(params.t0)) / params.step;
    generate_hotp(&secret, counter, params.digits)
}

/// Generate an HOTP code per RFC 4226.
pub fn generate_hotp(secret: &[u8], counter: u64, digits: u32) -> Result<String, TotpError> {
    if secret.is_empty() {
        return Err(TotpError::EmptySecret);
    }

    // Step 1: HMAC-SHA1
    let mut mac = HmacSha1::new_from_slice(secret).map_err(|_| TotpError::HmacError)?;
    mac.update(&counter.to_be_bytes());
    let result = mac.finalize().into_bytes();

    // Step 2: Dynamic truncation (RFC 4226 §5.3)
    let offset = (result[19] & 0x0f) as usize;
    let code = u32::from_be_bytes([
        result[offset] & 0x7f,
        result[offset + 1],
        result[offset + 2],
        result[offset + 3],
    ]);

    // Step 3: Modulo and zero-pad
    let modulus = 10u32.pow(digits);
    let otp = code % modulus;
    Ok(format!("{:0>width$}", otp, width = digits as usize))
}

/// Decode a base32-encoded string (RFC 4648, case-insensitive, strips padding/spaces).
fn decode_base32(input: &str) -> Result<Vec<u8>, TotpError> {
    // Strip spaces and padding, uppercase
    let cleaned: String = input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '=')
        .collect::<String>()
        .to_ascii_uppercase();

    if cleaned.is_empty() {
        return Err(TotpError::InvalidBase32);
    }

    // Use data-encoding for proper base32 decoding
    data_encoding::BASE32_NOPAD
        .decode(cleaned.as_bytes())
        .map_err(|_| TotpError::InvalidBase32)
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 4226 Appendix D test vectors
    // Secret: "12345678901234567890" (ASCII bytes, not base32)
    // Base32 of "12345678901234567890" = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"
    const TEST_SECRET_BASE32: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";
    const TEST_SECRET_RAW: &[u8] = b"12345678901234567890";

    #[test]
    fn hotp_rfc4226_appendix_d() {
        // RFC 4226 Appendix D: expected HOTP values for counters 0-9
        let expected = [
            "755224", "287082", "359152", "969429", "338314", "254676", "287922", "162583",
            "399871", "520489",
        ];
        for (counter, expected_code) in expected.iter().enumerate() {
            let code = generate_hotp(TEST_SECRET_RAW, counter as u64, 6).unwrap();
            assert_eq!(&code, expected_code, "HOTP mismatch at counter {counter}");
        }
    }

    #[test]
    fn totp_rfc6238_sha1_vectors() {
        // RFC 6238 Appendix B test vectors (SHA-1 only)
        let params = TotpParams {
            step: 30,
            digits: 8,
            t0: 0,
        };
        let cases = [
            (59u64, "94287082"),
            (1111111109, "07081804"),
            (1111111111, "14050471"),
            (1234567890, "89005924"),
            (2000000000, "69279037"),
            (20000000000, "65353130"),
        ];
        for (time, expected) in cases {
            let code = generate_totp(TEST_SECRET_BASE32, time, &params).unwrap();
            assert_eq!(code, expected, "TOTP mismatch at time {time}");
        }
    }

    #[test]
    fn totp_default_6_digits() {
        let params = TotpParams::default();
        let code = generate_totp(TEST_SECRET_BASE32, 59, &params).unwrap();
        assert_eq!(code.len(), 6);
        // Counter = 59/30 = 1, HOTP(1) = 287082
        assert_eq!(code, "287082");
    }

    #[test]
    fn totp_zero_padding() {
        // Ensure codes with leading zeros are properly padded
        let params = TotpParams {
            step: 30,
            digits: 8,
            t0: 0,
        };
        let code = generate_totp(TEST_SECRET_BASE32, 1111111109, &params).unwrap();
        assert_eq!(code, "07081804"); // Leading zero preserved
    }

    #[test]
    fn invalid_base32_error() {
        let params = TotpParams::default();
        let result = generate_totp("!!!invalid!!!", 0, &params);
        assert!(matches!(result, Err(TotpError::InvalidBase32)));
    }

    #[test]
    fn empty_secret_error() {
        let params = TotpParams::default();
        let result = generate_totp("", 0, &params);
        assert!(matches!(result, Err(TotpError::EmptySecret)));
    }

    #[test]
    fn whitespace_only_secret_error() {
        let params = TotpParams::default();
        let result = generate_totp("   ", 0, &params);
        assert!(matches!(result, Err(TotpError::InvalidBase32)));
    }

    #[test]
    fn base32_with_spaces_and_padding() {
        // Authenticator apps sometimes show secrets with spaces
        let params = TotpParams::default();
        let code_clean = generate_totp("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ", 59, &params).unwrap();
        let code_spaced =
            generate_totp("GEZD GNBV GY3T QOJQ GEZD GNBV GY3T QOJQ", 59, &params).unwrap();
        let code_padded =
            generate_totp("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ====", 59, &params).unwrap();
        assert_eq!(code_clean, code_spaced);
        assert_eq!(code_clean, code_padded);
    }

    #[test]
    fn base32_case_insensitive() {
        let params = TotpParams::default();
        let upper = generate_totp("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ", 59, &params).unwrap();
        let lower = generate_totp("gezdgnbvgy3tqojqgezdgnbvgy3tqojq", 59, &params).unwrap();
        assert_eq!(upper, lower);
    }

    #[test]
    fn decode_base32_roundtrip() {
        let decoded = decode_base32("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ").unwrap();
        assert_eq!(decoded, b"12345678901234567890");
    }
}
