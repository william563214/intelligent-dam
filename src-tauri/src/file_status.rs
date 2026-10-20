//! Files.status 狀態機常數與輕量 helper
//! processing / ready / scrape_failed / missing / suspected_duplicate

#[allow(non_upper_case_globals)]
pub const processing: &str = "processing";
#[allow(non_upper_case_globals)]
pub const ready: &str = "ready";
#[allow(non_upper_case_globals)]
pub const scrape_failed: &str = "scrape_failed";
#[allow(non_upper_case_globals)]
pub const missing: &str = "missing";
#[allow(non_upper_case_globals)]
pub const suspected_duplicate: &str = "suspected_duplicate";

/// 已知狀態集合 用於驗證與測試
pub fn all_statuses() -> &'static [&'static str] {
    &[
        processing,
        ready,
        scrape_failed,
        missing,
        suspected_duplicate,
    ]
}

pub fn is_known(status: &str) -> bool {
    all_statuses().contains(&status)
}

/// 預覽等工作結束後應回到的狀態
/// scrape_failed / suspected_duplicate 保留 其餘成功則 ready
pub fn after_transient_work(previous: &str) -> &'static str {
    if previous == scrape_failed {
        scrape_failed
    } else if previous == suspected_duplicate {
        suspected_duplicate
    } else {
        ready
    }
}

/// phash／重掃描背景工作可否覆寫為 processing
/// scrape_failed / suspected_duplicate / missing 必須保留
pub fn allows_transient_processing(status: &str) -> bool {
    status != scrape_failed && status != missing && status != suspected_duplicate
}

/// True when auto pipeline / organize must not run for this status.
pub fn blocks_auto_pipeline(status: &str) -> bool {
    status == missing || status == suspected_duplicate
}

/// 合法狀態轉換白名單 同狀態視為合法（冪等）
/// scrape_failed → processing 僅供刮削重試 呼叫端勿用於 phash
pub fn can_transition(from: &str, to: &str) -> bool {
    if !is_known(from) || !is_known(to) {
        return false;
    }
    if from == to {
        return true;
    }
    match (from, to) {
        // 任何狀態都可因缺檔進入 missing
        (_, s) if s == missing => true,
        // missing 復現僅回 ready（失敗歷史不保留 見 SPEC）
        (s, t) if s == missing && t == ready => true,
        // 背景／刮削進入 processing（scrape_failed 僅刮削重試）
        (s, t) if t == processing && (s == ready || s == scrape_failed) => true,
        // phash near-hit → 疑似重複（可自 processing 或 ready）
        (s, t)
            if t == suspected_duplicate && (s == ready || s == processing) =>
        {
            true
        }
        // 使用者忽略疑似重複 → ready 後可再進流水線
        (s, t) if s == suspected_duplicate && t == ready => true,
        // 工作結束或刮削結果
        (s, t)
            if s == processing && (t == ready || t == scrape_failed) =>
        {
            true
        }
        (s, t)
            if (s == ready || s == scrape_failed) && (t == ready || t == scrape_failed) =>
        {
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_statuses_cover_spec() {
        assert!(is_known(processing));
        assert!(is_known(ready));
        assert!(is_known(scrape_failed));
        assert!(is_known(missing));
        assert!(is_known(suspected_duplicate));
        assert!(!is_known("unknown"));
    }

    #[test]
    fn after_transient_work_preserves_failure_and_suspected() {
        assert_eq!(after_transient_work(scrape_failed), scrape_failed);
        assert_eq!(after_transient_work(suspected_duplicate), suspected_duplicate);
        assert_eq!(after_transient_work(processing), ready);
        assert_eq!(after_transient_work(ready), ready);
        assert_eq!(after_transient_work(missing), ready);
    }

    #[test]
    fn allows_transient_processing_skips_failure_states() {
        assert!(allows_transient_processing(ready));
        assert!(allows_transient_processing(processing));
        assert!(!allows_transient_processing(scrape_failed));
        assert!(!allows_transient_processing(missing));
        assert!(!allows_transient_processing(suspected_duplicate));
    }

    #[test]
    fn blocks_auto_pipeline_for_suspected_and_missing() {
        assert!(blocks_auto_pipeline(suspected_duplicate));
        assert!(blocks_auto_pipeline(missing));
        assert!(!blocks_auto_pipeline(ready));
        assert!(!blocks_auto_pipeline(processing));
    }

    #[test]
    fn can_transition_whitelist() {
        assert!(can_transition(ready, processing));
        assert!(can_transition(processing, ready));
        assert!(can_transition(processing, scrape_failed));
        assert!(can_transition(scrape_failed, processing));
        assert!(can_transition(scrape_failed, ready));
        assert!(can_transition(ready, missing));
        assert!(can_transition(missing, ready));
        assert!(can_transition(scrape_failed, scrape_failed));
        assert!(can_transition(ready, suspected_duplicate));
        assert!(can_transition(processing, suspected_duplicate));
        assert!(can_transition(suspected_duplicate, ready));
        assert!(can_transition(suspected_duplicate, missing));
        assert!(!can_transition(missing, processing));
        assert!(!can_transition(missing, scrape_failed));
        assert!(!can_transition(suspected_duplicate, processing));
        assert!(!can_transition(ready, "bogus"));
        assert!(!can_transition("bogus", ready));
    }
}
