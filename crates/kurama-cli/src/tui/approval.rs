use kurama_protocol::policy::ApprovalRequest;

#[derive(Debug, Clone)]
pub struct ApprovalState {
    pub request: ApprovalRequest,
    pub arguments: serde_json::Value,
    pub editor: String,
    pub editing: bool,
}

impl ApprovalState {
    pub fn new(request: ApprovalRequest) -> Self {
        let arguments = request.arguments.clone();
        let editor = serde_json::to_string_pretty(&arguments).unwrap_or_else(|_| "{}".into());
        Self {
            request,
            arguments,
            editor,
            editing: false,
        }
    }
}
