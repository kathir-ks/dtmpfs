use subtle::ConstantTimeEq;
use tonic::{Request, Status};

pub fn check_token(req: Request<()>, expected: &str) -> Result<Request<()>, Status> {
    let got = req
        .metadata()
        .get("cluster-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if got.as_bytes().ct_eq(expected.as_bytes()).into() {
        Ok(req)
    } else {
        Err(Status::unauthenticated("invalid cluster token"))
    }
}
