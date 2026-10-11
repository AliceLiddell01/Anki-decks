import { useEffect, useState } from "react";
export type Availability =
  | "available"
  | "missing"
  | "incomplete"
  | "invalid"
  | "unavailable"
  | "not_observed";
export type Json =
  null | boolean | number | string | Json[] | { [key: string]: Json };
export interface Provenance {
  path: string;
  sha256: string | null;
  modified_unix_ms: number | null;
}
export interface Envelope<T> {
  api_version: 1;
  state: Availability;
  data: T | null;
  provenance: Provenance[];
  diagnostics: string[];
}
export interface Page<T> {
  items: T[];
  total: number;
  page: number;
  limit: number;
}
export interface TriageSummary {
  total_candidates: number;
  reviewed_candidates: number;
  unreviewed_candidates: number;
  individual_review: {
    decision_count: number;
    by_disposition: Record<string, number>;
  };
  group_review: {
    decisions: { decision_count: number };
    covered_candidate_ids: number;
    representative_candidate_ids: number;
  };
  findings: { total_findings: number; by_severity: Record<string, number> };
}
export interface QueueSummary {
  raw_candidates: number;
  review_units: number;
  individual_units: number;
  group_units: number;
  grouped_candidates: number;
  representative_candidates: number;
  unknown_candidates: number;
  units_by_priority: Record<string, number>;
  by_detector: Record<string, number>;
}
export interface ReviewSummary {
  id: string;
  namespace: string;
  head_sha: string | null;
  base_sha: string | null;
  workspace_variant: string;
  updated_unix_ms: number | null;
  candidate_count: number | null;
  unit_count: number | null;
  finding_count: number | null;
  coverage: TriageSummary | null;
  queue_summary: QueueSummary | null;
  artifacts: Record<string, Availability>;
  state: Availability;
}
export interface Health {
  components: { name: string; state: Availability; diagnostics: string[] }[];
  read_only: boolean;
  poll_interval_ms: number;
  limitations: string[];
}
export interface Generation {
  revision: number;
  trusted_reviews: number;
  quarantined_reviews: number;
  trusted_units: number;
}
export interface Pattern {
  key: Record<string, string>;
  signature: string;
  support: Record<string, Json>;
}
export interface LearningSummary {
  generation: Generation;
  schema_version: number;
  policy_version: number;
  counts: Record<string, number>;
  patterns: {
    abstained: boolean;
    rules_total: number;
    policy: string;
    limitations: string[];
    rules: Pattern[];
  };
}
export interface Snapshot {
  revision: string | number;
  observed_unix_ms: number;
  reviews: ReviewSummary[];
  learning: Envelope<LearningSummary>;
  health: Health;
}
export interface ReviewDetail {
  summary: ReviewSummary;
  target: Record<string, Json> | null;
  queue_summary: QueueSummary | null;
  coverage: TriageSummary | null;
  tool_runs:
    | {
        tool: string;
        status: string;
        message: string | null;
        diagnostic_count: number;
      }[]
    | null;
  pipeline: { name: string; state: Availability; description: string }[];
  limitations: string[];
}
export interface CandidateRow {
  id: string;
  detector: string;
  path: string;
  line: number | null;
  origin: string;
  unit_id: string | null;
  priority: string | null;
  role: string | null;
  unknown: boolean | null;
  disposition: string | null;
  surfaces: string[] | null;
}
export interface CandidateEvidence {
  id: string;
  detector: string;
  path: string;
  line: number | null;
  origin: string;
  column: number | null;
  snippet: string | null;
  signals: string[];
  source: string;
  metadata: Record<string, Json>;
}
export interface Unit {
  id: string;
  kind: "individual" | "group";
  priority: string;
  detector: string;
  role: string;
  candidate_count: number;
  representative_candidate_ids: string[];
  unknown: boolean;
  classification: Record<string, Json>;
}
export interface Finding {
  id: string;
  severity: string;
  title: string;
  description: string;
  provenance: string;
  candidate_ids: string[];
  candidate_count?: number;
  candidate_ids_truncated?: boolean;
}
export interface Decision {
  candidate_id?: string;
  id?: string;
  disposition: string;
  reason_code: string;
  explanation: string;
  finding_ids: string[];
  candidate_count?: number;
  representative_candidate_ids?: string[];
}
export interface CandidateDetail {
  candidate: CandidateEvidence;
  classification: Record<string, Json> | null;
  unit: Unit | null;
  decision: Decision | null;
  findings: Finding[] | null;
  triage_state: Availability;
  source_url: string | null;
  source_location: {
    snapshot_sha: string | null;
    path: string;
    line: number | null;
    state: Availability;
    limitation: string;
    file_status?: string | null;
  } | null;
}
export interface Triage {
  coverage: TriageSummary;
  findings: Page<Finding>;
  individual_decisions: Page<Decision>;
  group_decisions: Page<Decision>;
}
export interface FindingDetail {
  finding: Finding;
  covered_candidates: Page<CandidateRow>;
}
export interface GroupDetail {
  decision: Decision;
  covered_candidates: Page<CandidateRow>;
}
export interface Job {
  id: string;
  review_id: string;
  mode: string | null;
  scope: string | null;
  lifecycle: string | null;
  status: string | null;
  duration_ms: number | null;
  exit: Record<string, Json> | null;
  workspace_removed: boolean | null;
  state: Availability;
  limitations: string[];
  enforcement?: Record<string, Json> | null;
  cleanup?: string | null;
  request?: {
    argv: string[];
    cwd: string;
    options: Record<string, Json>;
  } | null;
}
export interface ImportRecord {
  review_id: string;
  head_sha: string;
  trust: string;
  outcome: string;
  revision: number;
  imported_at: number;
  workspace_label: string | null;
  limitations: string[];
  observations: Record<string, Json>;
}
export interface LearningRecord {
  id?: string;
  review_id?: string;
  signature?: string;
  key?: Record<string, string>;
  support?: Record<string, Json>;
  [key: string]: Json | undefined;
}
export function useApi<T>(path: string | null, revision: number = 0) {
  const [value, setValue] = useState<Envelope<T> | null>(null);
  const [loading, setLoading] = useState(Boolean(path));
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    if (!path) {
      setValue(null);
      setLoading(false);
      return;
    }
    const controller = new AbortController();
    setLoading(true);
    setError(null);
    setValue(null);
    fetch("/api/v1" + path, {
      signal: controller.signal,
      headers: { Accept: "application/json" },
    })
      .then(async (response) => {
        if (!response.ok)
          throw new Error(`HTTP ${response.status}: источник недоступен`);
        const result = (await response.json()) as Envelope<T>;
        if (result.api_version !== 1 || !("state" in result))
          throw new Error("Несовместимый контракт API");
        setValue(result);
      })
      .catch((error) => {
        if (error.name !== "AbortError") setError(error.message);
      })
      .finally(() => {
        if (!controller.signal.aborted) setLoading(false);
      });
    return () => controller.abort();
  }, [path, revision]);
  return { value, loading, error };
}
export function useLive() {
  const [state, setState] = useState("connecting");
  const [revision, setRevision] = useState(0);
  useEffect(() => {
    const source = new EventSource("/api/v1/events");
    source.onopen = () => {
      setState("connected");
      setRevision((x) => x + 1);
    };
    source.onerror = () => setState("disconnected");
    const update = () => setRevision((x) => x + 1);
    source.addEventListener("snapshot_changed", update);
    source.addEventListener("snapshot", update);
    return () => source.close();
  }, []);
  return { state, revision };
}
