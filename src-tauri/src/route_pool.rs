//! 出口线路池：记录每条线路在每个 Apple 站点上的保护冷却，并挑选下一条线路。
//!
//! Apple 按出口 IP 计数，连续约 30 次请求后会以 HTTP 541 拒绝十几分钟。切换
//! 线路只有在「被拦的线路先休息」时才有意义；否则只会把每个 IP 依次烧完。
//! 这里只做记账和选择，不发任何网络请求，也不认识 Clash —— 线路只是一个名字。
//!
//! 选择策略是「粘住当前线路」：同一出口配合同一套浏览器 Cookie 持续访问最像真实
//! 用户，每次切换还要重新建立连接和页面校验。只有当前线路进入冷却时才换下一条。

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// 冷却档位对应时长。Apple 拒绝固定等级 4；连接失败和请求次数阈值固定等级 1。
pub const COOLDOWN_STEPS: [Duration; 4] = [
    Duration::from_secs(5 * 60),
    Duration::from_secs(10 * 60),
    Duration::from_secs(20 * 60),
    Duration::from_secs(30 * 60),
];

/// 节点连接失败时，整个节点进入等级 1 冷却 5 分钟。
pub const UNREACHABLE_COOLDOWN: Duration = Duration::from_secs(5 * 60);

/// 同一节点 + Apple 站点累计实际请求达到 5 次后，进入等级 1 冷却 5 分钟。
pub const USES_BEFORE_COOLDOWN: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cooldown {
    level: usize,
    until: Instant,
}

/// 线路池。`site` 是 Apple 站点主机名，例如 `www.apple.com.cn`。
#[derive(Debug, Default)]
pub struct RoutePool {
    routes: Vec<String>,
    active: Option<String>,
    cooldowns: HashMap<(String, String), Cooldown>,
    /// 每个节点 + Apple 站点的累计实际请求次数（触发阈值后清零）。
    usage_counts: HashMap<(String, String), u32>,
    /// 连不通的节点，到期前对所有站点都跳过。
    unreachable: HashMap<String, Instant>,
}

impl RoutePool {
    /// 用最新的线路列表替换旧列表。仍存在的线路保留冷却记录，不能因为重新读取
    /// 节点列表就让刚被拦的节点立即复用。
    pub fn set_routes(&mut self, routes: Vec<String>) {
        self.cooldowns
            .retain(|(route, _), _| routes.iter().any(|name| name == route));
        self.usage_counts
            .retain(|(route, _), _| routes.iter().any(|name| name == route));
        self.unreachable.retain(|route, _| routes.contains(route));
        if self
            .active
            .as_ref()
            .is_some_and(|active| !routes.contains(active))
        {
            self.active = None;
        }
        self.routes = routes;
    }

    pub fn routes(&self) -> &[String] {
        &self.routes
    }

    /// 当前节点在指定 Apple 站点已连续使用的实际请求次数。
    pub fn usage_count(&self, route: &str, site: &str) -> u32 {
        self.usage_counts
            .get(&(route.to_string(), site.to_string()))
            .copied()
            .unwrap_or_default()
    }

    pub fn active(&self) -> Option<&str> {
        self.active.as_deref()
    }

    pub fn set_active(&mut self, route: Option<String>) {
        self.active = route.filter(|route| self.routes.contains(route));
    }

    fn remaining(&self, route: &str, site: &str, now: Instant) -> Option<Duration> {
        self.cooldowns
            .get(&(route.to_string(), site.to_string()))
            .map(|cooldown| cooldown.until.saturating_duration_since(now))
            .filter(|remaining| !remaining.is_zero())
    }

    pub fn is_available(&self, route: &str, site: &str, now: Instant) -> bool {
        self.remaining(route, site, now).is_none()
            && self
                .unreachable
                .get(route)
                .is_none_or(|until| *until <= now)
    }

    /// 节点连不通：短暂跳过，不计入 Apple 的冷却档位。
    pub fn mark_unreachable(&mut self, route: &str, now: Instant) {
        self.unreachable
            .insert(route.to_string(), now + UNREACHABLE_COOLDOWN);
    }

    /// 选出这次请求该用的线路：当前线路可用就继续用；否则从它之后按顺序找第一条
    /// 可用线路。全部冷却时返回 `None`。
    pub fn choose(&self, site: &str, now: Instant) -> Option<&str> {
        self.choose_except(site, now, &HashSet::new())
    }

    /// 与 [`Self::choose`] 相同，但跳过 `skip` 中的线路。
    ///
    /// 连不通的标记只有 [`UNREACHABLE_COOLDOWN`]，节点很多时逐个测完已经超过这个
    /// 时间，最先跳过的节点会重新变成可用。调用方用 `skip` 记下本轮已经测不通的
    /// 节点，才不会绕回去重测而永远走不到后面的节点。
    pub fn choose_except(&self, site: &str, now: Instant, skip: &HashSet<String>) -> Option<&str> {
        let usable = |route: &str| !skip.contains(route) && self.is_available(route, site, now);
        if let Some(active) = self.active()
            && usable(active)
        {
            return Some(active);
        }
        let start = self
            .active
            .as_ref()
            .and_then(|active| self.routes.iter().position(|route| route == active))
            .map_or(0, |index| index + 1);
        (0..self.routes.len())
            .map(|offset| &self.routes[(start + offset) % self.routes.len()])
            .find(|route| usable(route))
            .map(String::as_str)
    }

    /// Apple 返回 403、429、541 时直接进入等级 4（30 分钟）。
    /// 已处于等级 4 冷却时，重复并发拒绝不延长冷却时间。
    pub fn block(&mut self, route: &str, site: &str, now: Instant) -> Duration {
        let key = (route.to_string(), site.to_string());
        if let Some(cooldown) = self.cooldowns.get(&key)
            && cooldown.level == COOLDOWN_STEPS.len() - 1
            && cooldown.until > now
        {
            return cooldown.until.saturating_duration_since(now);
        }
        let duration = COOLDOWN_STEPS[COOLDOWN_STEPS.len() - 1];
        self.cooldowns.insert(
            key,
            Cooldown {
                level: COOLDOWN_STEPS.len() - 1,
                until: now + duration,
            },
        );
        duration
    }

    /// 记录一次已经实际发起的请求。每个节点 + 站点独立计数；累计达到 5 次后清零并进入等级 1（5 分钟）。
    /// 若同一次请求已触发 Apple 等级 4 拒绝冷却，等级 1 不得覆盖等级 4。
    pub fn record_usage(&mut self, route: &str, site: &str, now: Instant) -> bool {
        let key = (route.to_string(), site.to_string());
        let count = self.usage_counts.entry(key.clone()).or_default();
        *count = count.saturating_add(1);
        if *count < USES_BEFORE_COOLDOWN {
            return false;
        }
        self.usage_counts.insert(key.clone(), 0);
        let has_active_level_four = self.cooldowns.get(&key).is_some_and(|cooldown| {
            cooldown.level == COOLDOWN_STEPS.len() - 1 && cooldown.until > now
        });
        if !has_active_level_four {
            self.cooldowns.insert(
                key,
                Cooldown {
                    level: 0,
                    until: now + COOLDOWN_STEPS[0],
                },
            );
        }
        true
    }

    /// 线路在该站点拿到了正常响应，清除冷却档位。
    pub fn succeed(&mut self, route: &str, site: &str) {
        self.cooldowns
            .remove(&(route.to_string(), site.to_string()));
    }

    /// 该站点最早恢复可用的等待时间；已有可用线路时为 `None`。
    pub fn wait_for(&self, site: &str, now: Instant) -> Option<Duration> {
        if self.routes.is_empty() || self.choose(site, now).is_some() {
            return None;
        }
        self.routes
            .iter()
            .filter_map(|route| {
                let unreachable = self
                    .unreachable
                    .get(route)
                    .map(|until| until.saturating_duration_since(now))
                    .filter(|remaining| !remaining.is_zero());
                self.remaining(route, site, now).max(unreachable)
            })
            .min()
    }

    /// 用户手动指定节点：清除它在所有站点上的冷却和短暂跳过。
    pub fn clear_route(&mut self, route: &str) {
        self.cooldowns.retain(|(name, _), _| name != route);
        self.usage_counts.retain(|(name, _), _| name != route);
        self.unreachable.remove(route);
    }

    /// 返回节点当前有效的最高冷却等级及该记录的剩余时间。
    /// 同一节点可能在多个 Apple 站点分别冷却，界面展示等级最高的一项。
    pub fn cooldown_status(&self, route: &str, now: Instant) -> Option<(usize, Duration)> {
        let site_cooldown = self.cooldowns
            .iter()
            .filter(|((name, _), cooldown)| name == route && cooldown.until > now)
            .map(|(_, cooldown)| (cooldown.level + 1, cooldown.until.saturating_duration_since(now)))
            .max_by_key(|(level, remaining)| (*level, *remaining));
        let unreachable = self.unreachable
            .get(route)
            .map(|until| until.saturating_duration_since(now))
            .filter(|remaining| !remaining.is_zero())
            .map(|remaining| (1, remaining));
        site_cooldown.into_iter().chain(unreachable).max_by_key(|(level, remaining)| (*level, *remaining))
    }

    /// 节点暂时无法连接时的剩余跳过时间。
    pub fn unreachable_remaining(&self, route: &str, now: Instant) -> Option<Duration> {
        self.unreachable
            .get(route)
            .map(|until| until.saturating_duration_since(now))
            .filter(|remaining| !remaining.is_zero())
    }

    /// 线路是否在任一站点冷却或暂时连不通。
    pub fn is_resting(&self, route: &str, now: Instant) -> bool {
        self.cooldown_status(route, now).is_some()
            || self.unreachable_remaining(route, now).is_some()
    }

    /// 用户要求立即重试：结束所有等待，但保留档位。探测若仍被拒，会继续升档。
    pub fn release_waits(&mut self, now: Instant) {
        for cooldown in self.cooldowns.values_mut() {
            cooldown.until = cooldown.until.min(now);
        }
        self.unreachable.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SITE: &str = "www.apple.com.cn";

    fn pool(names: &[&str]) -> RoutePool {
        let mut pool = RoutePool::default();
        pool.set_routes(names.iter().map(|name| name.to_string()).collect());
        pool
    }

    #[test]
    fn 当前线路可用时保持不变() {
        let mut pool = pool(&["香港01", "日本01", "新加坡01"]);
        pool.set_active(Some("日本01".into()));
        assert_eq!(pool.choose(SITE, Instant::now()), Some("日本01"));
    }

    #[test]
    fn 当前线路被拦后按顺序换到下一条可用线路() {
        let now = Instant::now();
        let mut pool = pool(&["香港01", "日本01", "新加坡01"]);
        pool.set_active(Some("日本01".into()));
        pool.block("日本01", SITE, now);
        pool.block("新加坡01", SITE, now);
        assert_eq!(pool.choose(SITE, now), Some("香港01"));
    }

    #[test]
    fn 冷却按站点区分() {
        let now = Instant::now();
        let mut pool = pool(&["香港01"]);
        pool.set_active(Some("香港01".into()));
        pool.block("香港01", SITE, now);
        assert_eq!(pool.choose(SITE, now), None);
        assert_eq!(pool.choose("www.apple.com", now), Some("香港01"));
    }

    #[test]
    fn Apple拒绝始终直接进入等级四() {
        let now = Instant::now();
        let mut pool = pool(&["香港01"]);
        assert_eq!(pool.block("香港01", SITE, now), COOLDOWN_STEPS[3]);
        let later = now + Duration::from_secs(60);
        assert_eq!(pool.block("香港01", SITE, later), COOLDOWN_STEPS[3] - Duration::from_secs(60));
        let after = now + COOLDOWN_STEPS[3];
        assert_eq!(pool.block("香港01", SITE, after), COOLDOWN_STEPS[3]);
    }

    #[test]
    fn 同一节点同一站点累计五次请求后进入等级一并重置计数() {
        let now = Instant::now();
        let mut pool = pool(&["香港01"]);
        for _ in 0..USES_BEFORE_COOLDOWN - 1 {
            assert!(!pool.record_usage("香港01", SITE, now));
        }
        assert_eq!(pool.usage_count("香港01", SITE), USES_BEFORE_COOLDOWN - 1);
        assert!(pool.is_available("香港01", SITE, now));
        assert!(pool.record_usage("香港01", SITE, now));
        assert_eq!(pool.usage_count("香港01", SITE), 0);
        assert!(!pool.is_available("香港01", SITE, now));
        assert_eq!(
            pool.remaining("香港01", SITE, now),
            Some(COOLDOWN_STEPS[0])
        );
        // 另一个站点的计数相互独立。
        assert!(!pool.record_usage("香港01", "www.apple.com", now));
        assert!(pool.is_available("香港01", "www.apple.com", now));
        // 等级 1 冷却结束后从零重新累计。
        let after = now + COOLDOWN_STEPS[3];
        for _ in 0..USES_BEFORE_COOLDOWN - 1 {
            assert!(!pool.record_usage("香港01", SITE, after));
        }
        assert!(pool.is_available("香港01", SITE, after));
    }

    #[test]
    fn Apple拒绝冷却结束后再次拒绝仍然直接等级四() {
        let now = Instant::now();
        let mut pool = pool(&["香港01"]);
        pool.block("香港01", SITE, now);
        pool.succeed("香港01", SITE);
        assert_eq!(pool.block("香港01", SITE, now), COOLDOWN_STEPS[3]);
    }

    #[test]
    fn 全部冷却时给出最早恢复时间() {
        let now = Instant::now();
        let mut pool = pool(&["香港01", "日本01"]);
        pool.block("香港01", SITE, now);
        pool.block("日本01", SITE, now + Duration::from_secs(120));
        assert_eq!(pool.wait_for(SITE, now), Some(COOLDOWN_STEPS[3]));
        pool.set_routes(vec!["香港01".into(), "日本01".into(), "美国01".into()]);
        assert_eq!(pool.wait_for(SITE, now), None);
    }

    #[test]
    fn 重新读取节点列表保留仍存在线路的冷却() {
        let now = Instant::now();
        let mut pool = pool(&["香港01", "日本01"]);
        pool.set_active(Some("日本01".into()));
        pool.block("香港01", SITE, now);
        pool.set_routes(vec!["香港01".into(), "新加坡01".into()]);
        assert!(!pool.is_available("香港01", SITE, now));
        assert_eq!(pool.active(), None);
        assert_eq!(pool.choose(SITE, now), Some("新加坡01"));
    }

    #[test]
    fn 立即重试结束等待但保留档位() {
        let now = Instant::now();
        let mut pool = pool(&["香港01"]);
        pool.block("香港01", SITE, now);
        pool.release_waits(now);
        assert!(pool.is_available("香港01", SITE, now));
        assert_eq!(pool.block("香港01", SITE, now), COOLDOWN_STEPS[3]);
    }

    #[test]
    fn 连不通的节点进入等级一冷却五分钟且对所有站点生效() {
        let now = Instant::now();
        let mut pool = pool(&["香港01", "日本01"]);
        pool.set_active(Some("香港01".into()));
        pool.mark_unreachable("香港01", now);
        assert_eq!(pool.choose(SITE, now), Some("日本01"));
        assert_eq!(pool.choose("www.apple.com", now), Some("日本01"));
        assert_eq!(
            pool.choose(SITE, now + UNREACHABLE_COOLDOWN),
            Some("香港01")
        );
        // 连不通不计入 Apple 冷却档位。
        assert_eq!(pool.block("香港01", SITE, now), COOLDOWN_STEPS[3]);
    }

    #[test]
    fn 跳过本轮测不通的节点即使短暂跳过已到期() {
        let now = Instant::now();
        let mut pool = pool(&["香港01", "日本01", "新加坡01"]);
        pool.set_active(Some("香港01".into()));
        pool.block("香港01", SITE, now);
        pool.mark_unreachable("日本01", now);
        let skip = HashSet::from(["日本01".to_string()]);
        let later = now + UNREACHABLE_COOLDOWN;
        // 短暂跳过已经到期，不排除的话会绕回去重测日本01。
        assert_eq!(pool.choose(SITE, later), Some("日本01"));
        assert_eq!(pool.choose_except(SITE, later, &skip), Some("新加坡01"));
        let skip = HashSet::from(["日本01".to_string(), "新加坡01".to_string()]);
        assert_eq!(pool.choose_except(SITE, later, &skip), None);
    }

    #[test]
    fn 全部节点都连不通时给出最早恢复时间() {
        let now = Instant::now();
        let mut pool = pool(&["香港01"]);
        pool.mark_unreachable("香港01", now);
        assert_eq!(pool.wait_for(SITE, now), Some(UNREACHABLE_COOLDOWN));
        pool.release_waits(now);
        assert_eq!(pool.choose(SITE, now), Some("香港01"));
    }

    #[test]
    fn 手动指定节点清除它的冷却() {
        let now = Instant::now();
        let mut pool = pool(&["香港01", "日本01"]);
        pool.block("香港01", SITE, now);
        pool.mark_unreachable("香港01", now);
        assert!(pool.is_resting("香港01", now));
        pool.clear_route("香港01");
        assert!(!pool.is_resting("香港01", now));
        assert!(pool.is_available("香港01", SITE, now));
    }
}
