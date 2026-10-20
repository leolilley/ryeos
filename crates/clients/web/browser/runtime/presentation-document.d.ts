export interface PresentationDocumentHandoff {
  readonly ready: Promise<void>;
  dispose(): void;
  settle(): Promise<void>;
}
export function preparedDocumentUrl(current: URL, presentationId: string): URL;
export function createPresentationDocumentHandoff(
  document: Document, parent: Element, current: URL,
  prepare: () => Promise<string>, onDismiss?: () => void,
): PresentationDocumentHandoff;
