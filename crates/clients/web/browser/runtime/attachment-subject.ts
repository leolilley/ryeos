/** Mechanical public DTO shape check only. Subject metadata never grants access. */
export function requireAttachmentSubject(attachment: Record<string, unknown>): void {
  const subject = attachment.subject;
  if (typeof subject !== "object" || subject === null || Array.isArray(subject)) {
    throw new Error("binding attachment subject is required");
  }
  const value = subject as Record<string, unknown>;
  const keys = Object.keys(value);
  const validText = (text: unknown): text is string => typeof text === "string"
    && text.length > 0 && text.trim() === text && !/[\u0000-\u001f\u007f-\u009f]/u.test(text);
  const project = validText(attachment.project_path);
  if (value.kind === "projectless" && keys.length === 1 && attachment.project_path === null) return;
  if (value.kind === "unregistered_project" && keys.length === 1 && project) return;
  if (value.kind === "registered_project" && keys.length === 2
      && keys.includes("local_id") && validText(value.local_id) && project) return;
  throw new Error("binding attachment subject contradicts its project metadata or closed schema");
}
