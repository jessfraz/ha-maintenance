#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("only the owner can manage standing instructions")]
    Forbidden,
    #[error("maintenance item not found")]
    NotFound,
    #[error("the maintenance item was permanently deleted")]
    Deleted,
    #[error("{0}")]
    Conflict(&'static str),
    #[error("the item changed; refresh before trying again")]
    Precondition,
    #[error("stored maintenance data is invalid")]
    Corrupt,
    #[error("database operation failed")]
    Database(#[from] sqlx::Error),
    #[error("stored maintenance data is invalid")]
    Json(#[from] serde_json::Error),
    #[error("database migration failed")]
    Migration(#[from] sqlx::migrate::MigrateError),
}

impl From<Error> for dropshot::HttpError {
    fn from(error: Error) -> Self {
        use dropshot::ClientErrorStatusCode as Status;
        let (status, code) = match error {
            Error::Invalid(_) => (Status::BAD_REQUEST, "invalid_input"),
            Error::Forbidden => (Status::FORBIDDEN, "owner_required"),
            Error::NotFound => (Status::NOT_FOUND, "not_found"),
            Error::Deleted => (Status::GONE, "item_deleted"),
            Error::Conflict(_) => (Status::CONFLICT, "conflict"),
            Error::Precondition => (Status::PRECONDITION_FAILED, "revision_conflict"),
            _ => return Self::for_internal_error(error.to_string()),
        };
        Self::for_client_error(Some(code.into()), status, error.to_string())
    }
}
