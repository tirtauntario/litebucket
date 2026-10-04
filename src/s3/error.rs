//! S3 error codes and their XML/HTTP representation.

use http::StatusCode;

use crate::error::Error;

#[derive(Debug, Clone)]
pub struct S3Error {
    pub code: &'static str,
    pub status: StatusCode,
    pub message: String,
    /// Extra XML elements (e.g. `Region`, `Condition`).
    pub extra: Vec<(&'static str, String)>,
    /// Extra response headers (e.g. `x-amz-bucket-region`, `Content-Range`).
    pub headers: Vec<(&'static str, String)>,
    /// Internal detail for logs only; never serialized.
    pub detail: Option<String>,
}

impl std::fmt::Display for S3Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for S3Error {}

pub type S3Result<T> = Result<T, S3Error>;

macro_rules! ctor {
    ($fn:ident, $code:literal, $status:ident, $msg:literal) => {
        pub fn $fn() -> Self {
            Self::new($code, StatusCode::$status, $msg)
        }
    };
}

impl S3Error {
    pub fn new(code: &'static str, status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            code,
            status,
            message: message.into(),
            extra: Vec::new(),
            headers: Vec::new(),
            detail: None,
        }
    }

    pub fn with_extra(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.extra.push((name, value.into()));
        self
    }

    pub fn with_header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn is_server_error(&self) -> bool {
        self.status.is_server_error()
    }

    ctor!(access_denied, "AccessDenied", FORBIDDEN, "Access Denied");
    ctor!(
        no_such_bucket,
        "NoSuchBucket",
        NOT_FOUND,
        "The specified bucket does not exist"
    );
    ctor!(
        no_such_key,
        "NoSuchKey",
        NOT_FOUND,
        "The specified key does not exist."
    );
    ctor!(
        no_such_upload,
        "NoSuchUpload",
        NOT_FOUND,
        "The specified upload does not exist. The upload ID may be invalid, or the upload may have been aborted or completed."
    );
    ctor!(
        bucket_not_empty,
        "BucketNotEmpty",
        CONFLICT,
        "The bucket you tried to delete is not empty"
    );
    ctor!(
        bucket_already_owned,
        "BucketAlreadyOwnedByYou",
        CONFLICT,
        "Your previous request to create the named bucket succeeded and you already own it."
    );
    ctor!(
        too_many_buckets,
        "TooManyBuckets",
        BAD_REQUEST,
        "You have attempted to create more buckets than allowed"
    );
    ctor!(
        malformed_xml,
        "MalformedXML",
        BAD_REQUEST,
        "The XML you provided was not well-formed or did not validate against our published schema"
    );
    ctor!(
        entity_too_large,
        "EntityTooLarge",
        BAD_REQUEST,
        "Your proposed upload exceeds the maximum allowed size"
    );
    ctor!(
        entity_too_small,
        "EntityTooSmall",
        BAD_REQUEST,
        "Your proposed upload is smaller than the minimum allowed object size."
    );
    ctor!(
        invalid_digest,
        "InvalidDigest",
        BAD_REQUEST,
        "The Content-MD5 you specified was invalid."
    );
    ctor!(
        bad_digest,
        "BadDigest",
        BAD_REQUEST,
        "The Content-MD5 you specified did not match what we received."
    );
    ctor!(
        precondition_failed,
        "PreconditionFailed",
        PRECONDITION_FAILED,
        "At least one of the pre-conditions you specified did not hold"
    );
    ctor!(
        invalid_part_order,
        "InvalidPartOrder",
        BAD_REQUEST,
        "The list of parts was not in ascending order. The parts list must be specified in order by part number."
    );
    ctor!(
        signature_does_not_match,
        "SignatureDoesNotMatch",
        FORBIDDEN,
        "The request signature we calculated does not match the signature you provided. Check your key and signing method."
    );
    ctor!(
        invalid_access_key_id,
        "InvalidAccessKeyId",
        FORBIDDEN,
        "The AWS Access Key Id you provided does not exist in our records."
    );
    ctor!(
        request_time_too_skewed,
        "RequestTimeTooSkewed",
        FORBIDDEN,
        "The difference between the request time and the current time is too large."
    );
    ctor!(
        missing_content_length,
        "MissingContentLength",
        LENGTH_REQUIRED,
        "You must provide the Content-Length HTTP header."
    );
    ctor!(
        incomplete_body,
        "IncompleteBody",
        BAD_REQUEST,
        "You did not provide the number of bytes specified by the Content-Length HTTP header."
    );
    ctor!(
        request_timeout,
        "RequestTimeout",
        BAD_REQUEST,
        "Your socket connection to the server was not read from or written to within the timeout period."
    );
    ctor!(
        slow_down,
        "SlowDown",
        SERVICE_UNAVAILABLE,
        "Please reduce your request rate."
    );
    ctor!(
        internal,
        "InternalError",
        INTERNAL_SERVER_ERROR,
        "We encountered an internal error. Please try again."
    );
    ctor!(
        content_sha256_mismatch,
        "XAmzContentSHA256Mismatch",
        BAD_REQUEST,
        "The provided 'x-amz-content-sha256' header does not match what was computed."
    );
    ctor!(
        no_such_cors,
        "NoSuchCORSConfiguration",
        NOT_FOUND,
        "The CORS configuration does not exist"
    );
    ctor!(
        method_not_allowed,
        "MethodNotAllowed",
        METHOD_NOT_ALLOWED,
        "The specified method is not allowed against this resource."
    );

    pub fn invalid_argument(msg: impl Into<String>) -> Self {
        Self::new("InvalidArgument", StatusCode::BAD_REQUEST, msg)
    }

    pub fn invalid_request(msg: impl Into<String>) -> Self {
        Self::new("InvalidRequest", StatusCode::BAD_REQUEST, msg)
    }

    pub fn invalid_bucket_name(msg: impl Into<String>) -> Self {
        Self::new("InvalidBucketName", StatusCode::BAD_REQUEST, msg)
    }

    pub fn invalid_part(msg: impl Into<String>) -> Self {
        Self::new("InvalidPart", StatusCode::BAD_REQUEST, msg)
    }

    pub fn not_implemented(msg: impl Into<String>) -> Self {
        Self::new("NotImplemented", StatusCode::NOT_IMPLEMENTED, msg)
    }

    pub fn service_unavailable(msg: impl Into<String>) -> Self {
        Self::new("ServiceUnavailable", StatusCode::SERVICE_UNAVAILABLE, msg)
    }

    pub fn bad_checksum(alg: &str) -> Self {
        Self::new(
            "BadDigest",
            StatusCode::BAD_REQUEST,
            format!("The {alg} you specified did not match the calculated checksum."),
        )
    }

    pub fn invalid_range(size: u64) -> Self {
        Self::new(
            "InvalidRange",
            StatusCode::RANGE_NOT_SATISFIABLE,
            "The requested range is not satisfiable",
        )
        .with_extra("ActualObjectSize", size.to_string())
        .with_header("content-range", format!("bytes */{size}"))
    }

    pub fn quota_exceeded() -> Self {
        Self::new(
            "QuotaExceeded",
            StatusCode::FORBIDDEN,
            "The bucket's configured storage quota would be exceeded.",
        )
    }

    pub fn insufficient_capacity() -> Self {
        Self::service_unavailable(
            "The server does not currently have enough storage capacity for this request.",
        )
    }

    pub fn authorization_query_error(msg: impl Into<String>) -> Self {
        Self::new(
            "AuthorizationQueryParametersError",
            StatusCode::BAD_REQUEST,
            msg,
        )
    }

    pub fn authorization_header_malformed(msg: impl Into<String>) -> Self {
        Self::new("AuthorizationHeaderMalformed", StatusCode::BAD_REQUEST, msg)
    }
}

impl From<Error> for S3Error {
    fn from(e: Error) -> Self {
        if e.is_no_space() {
            return S3Error::insufficient_capacity().with_detail(e.to_string());
        }
        match e {
            Error::Overloaded(what) => S3Error::slow_down().with_detail(what),
            Error::Halted(why) => {
                S3Error::service_unavailable("The service is temporarily refusing writes.")
                    .with_detail(why)
            }
            other => S3Error::internal().with_detail(other.to_string()),
        }
    }
}

impl From<crate::capacity::CapacityError> for S3Error {
    fn from(e: crate::capacity::CapacityError) -> Self {
        S3Error::insufficient_capacity().with_detail(format!("{e:?}"))
    }
}
