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
}
