//! Files.status 狀態機常數與輕量 helper
//! processing / ready / scrape_failed / missing

#[allow(non_upper_case_globals)]
pub const processing: &str = "processing";
#[allow(non_upper_case_globals)]
pub const ready: &str = "ready";
#[allow(non_upper_case_globals)]
pub const scrape_failed: &str = "scrape_failed";
#[allow(non_upper_case_globals)]
pub const missing: &str = "missing";

/// 已知狀態集合 用於驗證與測試
pub fn all_statuses() -> &'static [&'static str] {
    &[processing, ready, scrape_failed, missing]
}

pub fn is_known(status: &str) -> bool {
    all_statuses().contains(&status)
}

/// 預覽等工作結束後應回到的狀態
/// scrape_failed 保留 其餘成功則 ready
pub fn after_transient_work(previous: &str) -> &'static str {
    if previous == scrape_failed {
        scrape_failed
    } else {
        ready
    }
}

/// phash／重掃描背景工作可否覆寫為 processing
/// scrape_failed 必須保留 直到真正刮削成功
pub fn allows_transient_processing(status: &str) -> bool {
    status != scrape_failed && status != missing
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
        assert!(!is_known("unknown"));
    }

    #[test]
    fn after_transient_work_preserves_scrape_failed() {
        assert_eq!(after_transient_work(scrape_failed), scrape_failed);
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
        assert!(!can_transition(missing, processing));
        assert!(!can_transition(missing, scrape_failed));
        assert!(!can_transition(ready, "bogus"));
        assert!(!can_transition("bogus", ready));
    }
}
