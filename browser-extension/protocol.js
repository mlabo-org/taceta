export const SCHEMA_VERSION = 1;
export const PRODUCT_VERSION = "0.1.0";
export const PROTOCOL_VERSION = 1;
export const MESSAGE_TYPES = new Set(["request", "response", "event"]);
export const OPERATIONS = new Set(["ping", "extension_ready", "poll_job", "job_result", "cancel", "cancel_ack", "health", "version_failure"]);
export const MUTATION_STATES = new Set(["not_performed", "pending", "performed", "performed_or_unknown"]);
export const WORKFLOWS = new Set(["default_search", "google_search", "page_fetch", "chatgpt_web"]);
export function envelope(message_type, request_id, session_id, operation, payload = {}, mutation_state) {
  if (!MESSAGE_TYPES.has(message_type) || typeof request_id !== "string" || typeof session_id !== "string" || !OPERATIONS.has(operation)) throw new Error("invalid envelope");
  const m = {schema_version:SCHEMA_VERSION, product_version:PRODUCT_VERSION, protocol_version:PROTOCOL_VERSION, message_type, request_id, session_id, operation, payload};
  if (mutation_state !== undefined) m.mutation_state = mutation_state;
  return m;
}
export function validateEnvelope(message, expectedType) {
  if (!message || message.schema_version !== SCHEMA_VERSION || message.product_version !== PRODUCT_VERSION || message.protocol_version !== PROTOCOL_VERSION || !MESSAGE_TYPES.has(message.message_type) || (expectedType && message.message_type !== expectedType) || typeof message.request_id !== "string" || typeof message.session_id !== "string" || !OPERATIONS.has(message.operation) || !message.payload || typeof message.payload !== "object") throw new Error("invalid protocol envelope");
  if (message.mutation_state !== undefined && !MUTATION_STATES.has(message.mutation_state)) throw new Error("invalid mutation state");
  const allowed = new Set(["schema_version","product_version","protocol_version","message_type","request_id","session_id","operation","payload","mutation_state"]);
  if (Object.keys(message).some(k => !allowed.has(k))) throw new Error("unknown protocol field");
  return message;
}
export function validateJob(job, request) {
  if (!job || typeof job.job_id !== "string" || !WORKFLOWS.has(job.workflow) || !Number.isInteger(job.limit) || !Number.isInteger(job.timeout_ms)) throw new Error("invalid job");
  if (job.idle_timeout_ms != null && (!Number.isInteger(job.idle_timeout_ms) || job.idle_timeout_ms <= 0 || job.idle_timeout_ms > job.timeout_ms)) throw new Error("invalid idle timeout");
  const search = ["default_search", "google_search"].includes(job.workflow);
  const validInput = search
    ? typeof job.query === "string" && Boolean(job.query.trim()) && !job.url && !job.prompt
    : job.workflow === "page_fetch"
      ? typeof job.url === "string" && /^https:\/\//i.test(job.url) && !job.query && !job.prompt
      : typeof job.prompt === "string" && Boolean(job.prompt) && !job.query && !job.url;
  if (!validInput) throw new Error("invalid job input");
  const a=job.authorization; if (!a || a.kind !== "web_request" || a.request_id !== request.request_id || a.session_id !== request.session_id || a.once !== true) throw new Error("invalid authorization");
  return job;
}
export function jobResultPayload(job, status, data = {}) {
  if (!job || typeof job.job_id !== "string" || !WORKFLOWS.has(job.workflow) || !["completed","failed"].includes(status) || !data || typeof data !== "object") throw new Error("invalid job result");
  return {
    ...data,
    job_id: job.job_id,
    workflow: job.workflow,
    status,
    mutation_state: status === "completed" ? "performed" : job.workflow === "chatgpt_web" ? "performed_or_unknown" : "not_performed",
  };
}
