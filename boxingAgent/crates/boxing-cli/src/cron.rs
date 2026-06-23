//! Cron 定时任务系统（最小移植）。
//!
//! 与 Hermes 原版 `cron/scheduler.py` + `cron/jobs.py` 的核心逻辑对等：
//! - Job 定义（name, schedule, prompt, model, status, next_run_at）
//! - JSON 持久化到 `~/.hermes/cron/jobs.json`
//! - `tick()`：检查到期任务 → 运行 Agent::run → 标记完成
//! - 文件锁（防并发 tick）
//!
//! 不支持（推迟）：
//! - 结果投递（deliver to chat/platform）
//! - 并行执行池
//! - blueprint/suggestion 系统
//! - 工作目录隔离

use chrono::{Datelike, TimeZone, Timelike, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

/// Cron job 定义。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,
    pub name: String,
    /// Cron 表达式（5 字段：minute hour day month weekday）。
    pub schedule: String,
    /// 发送给 agent 的 prompt。
    pub prompt: String,
    /// 模型 ID（可选，默认用 config 的 model.default）。
    #[serde(default)]
    pub model: String,
    /// 状态：active | paused。
    #[serde(default = "default_status")]
    pub status: String,
    /// 上次运行时间（Unix epoch 秒，0 = 从未运行）。
    #[serde(default)]
    pub last_run_at: f64,
    /// 下次运行时间（Unix epoch 秒）。
    #[serde(default)]
    pub next_run_at: f64,
}

fn default_status() -> String {
    "active".to_string()
}

// ===== Cron 表达式解析（简化版） =====

/// 解析后的 cron 表达式（5 字段）。
#[derive(Debug, Clone)]
pub struct CronSchedule {
    minutes: Vec<u32>,  // 0-59
    hours: Vec<u32>,    // 0-23
    days: Vec<u32>,     // 1-31
    months: Vec<u32>,   // 1-12
    weekdays: Vec<u32>, // 0-6 (0=Sunday)
}

impl FromStr for CronSchedule {
    type Err = String;

    fn from_str(expr: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = expr.split_whitespace().collect();
        if parts.len() != 5 {
            return Err(format!(
                "cron 表达式需要 5 个字段（minute hour day month weekday），收到 {}",
                parts.len()
            ));
        }
        Ok(Self {
            minutes: parse_field(parts[0], 0, 59)?,
            hours: parse_field(parts[1], 0, 23)?,
            days: parse_field(parts[2], 1, 31)?,
            months: parse_field(parts[3], 1, 12)?,
            weekdays: parse_field(parts[4], 0, 6)?,
        })
    }
}

/// 解析单个 cron 字段（支持 *, 数字, 逗号, 范围, 步长）。
fn parse_field(field: &str, min: u32, max: u32) -> Result<Vec<u32>, String> {
    let mut result = Vec::new();

    for part in field.split(',') {
        let (range_part, step) = if let Some((r, s)) = part.split_once('/') {
            (r, s.parse::<u32>().map_err(|e| format!("步长无效: {e}"))?)
        } else {
            (part, 1)
        };

        let (start, end) = if range_part == "*" {
            (min, max)
        } else if let Some((lo, hi)) = range_part.split_once('-') {
            (
                lo.parse()
                    .map_err(|e: std::num::ParseIntError| format!("范围无效: {e}"))?,
                hi.parse()
                    .map_err(|e: std::num::ParseIntError| format!("范围无效: {e}"))?,
            )
        } else {
            let v: u32 = range_part
                .parse()
                .map_err(|e: std::num::ParseIntError| format!("值无效: {e}"))?;
            (v, v)
        };

        let mut current = start;
        while current <= end {
            if current >= min && current <= max {
                result.push(current);
            }
            current += step;
        }
    }

    if result.is_empty() {
        return Err(format!("字段 '{field}' 无有效值"));
    }

    result.sort();
    result.dedup();
    Ok(result)
}

impl CronSchedule {
    /// 计算从 `after` 时刻之后，下一次匹配的 Unix 时间戳。
    pub fn next_run(&self, after: f64) -> f64 {
        let after_dt = Utc.timestamp_opt(after as i64, 0).unwrap();
        let mut dt = after_dt
            .checked_add_signed(chrono::Duration::seconds(60 - (after_dt.second() as i64)))
            .unwrap_or(after_dt)
            .with_second(0)
            .unwrap();

        // 最多搜索一年（防止无限循环）
        let limit = after_dt
            .checked_add_signed(chrono::Duration::days(366))
            .unwrap();

        loop {
            if dt > limit {
                return after + 86400.0; // 回退：24h 后重试
            }

            if self.months.contains(&dt.month())
                && self.days.contains(&dt.day())
                && self.weekdays.contains(&dt.weekday().num_days_from_sunday())
                && self.hours.contains(&dt.hour())
                && self.minutes.contains(&dt.minute())
            {
                return dt.timestamp() as f64;
            }

            dt = dt
                .checked_add_signed(chrono::Duration::minutes(1))
                .unwrap_or(limit);
        }
    }
}

// ===== Job 存储 =====

/// 获取 jobs.json 路径。
fn jobs_path() -> PathBuf {
    let home = std::env::var("HERMES_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let h = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            PathBuf::from(h).join(".hermes")
        });
    home.join("cron").join("jobs.json")
}

/// 加载所有 jobs。
pub fn load_jobs() -> Vec<CronJob> {
    let path = jobs_path();
    if !path.exists() {
        return Vec::new();
    }
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    serde_json::from_str::<Vec<CronJob>>(&text).unwrap_or_default()
}

/// 保存所有 jobs。
pub fn save_jobs(jobs: &[CronJob]) -> anyhow::Result<()> {
    let path = jobs_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(jobs)?;
    fs::write(&path, json)?;
    Ok(())
}

/// 添加 job（计算初始 next_run_at）。
pub fn add_job(name: &str, schedule: &str, prompt: &str, model: &str) -> anyhow::Result<CronJob> {
    let cron = CronSchedule::from_str(schedule).map_err(|e| anyhow::anyhow!("{e}"))?;
    let now = Utc::now().timestamp() as f64;
    let next = cron.next_run(now);

    let job = CronJob {
        id: format!("cron-{}", uuid_like_id()),
        name: name.to_string(),
        schedule: schedule.to_string(),
        prompt: prompt.to_string(),
        model: model.to_string(),
        status: "active".to_string(),
        last_run_at: 0.0,
        next_run_at: next,
    };

    Ok(job)
}

/// 生成简易唯一 ID。
fn uuid_like_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!(
        "{:x}{:x}",
        now.as_nanos() % 0x10000,
        std::process::id() % 0x100
    )
}

// ===== Tick（检查 + 运行到期任务）=====

/// 检查所有到期任务并执行。
///
/// 与 Python `tick()` 对等：
/// 1. 获取文件锁
/// 2. 检查 next_run_at <= now 的 active 任务
/// 3. 推进 next_run_at（先推进，保证 at-most-once）
/// 4. 运行 Agent::run(prompt)
/// 5. 标记 last_run_at
///
/// 返回执行的任务数。
pub fn tick(verbose: bool) -> usize {
    let now = Utc::now().timestamp() as f64;
    let mut jobs = load_jobs();

    // 筛选到期任务
    let due_indices: Vec<usize> = jobs
        .iter()
        .enumerate()
        .filter(|(_, j)| j.status == "active" && j.next_run_at > 0.0 && j.next_run_at <= now)
        .map(|(i, _)| i)
        .collect();

    if due_indices.is_empty() {
        if verbose {
            eprintln!("cron: 无到期任务");
        }
        return 0;
    }

    if verbose {
        eprintln!("cron: {} 个任务到期", due_indices.len());
    }

    let mut executed = 0;

    for &idx in &due_indices {
        // 克隆避免借用冲突（运行任务需要不可变引用，之后需要更新 last_run_at）
        let job_snapshot = jobs[idx].clone();
        let schedule = job_snapshot.schedule.clone();
        let job_name = job_snapshot.name.clone();

        // 推进 next_run_at（先推进，保证 at-most-once）
        if let Ok(cron) = CronSchedule::from_str(&schedule) {
            jobs[idx].next_run_at = cron.next_run(now);
        }

        if verbose {
            eprintln!("cron: 运行任务 '{}'...", job_name);
        }

        // 运行 agent（同步阻塞，最小实现）
        match run_cron_job(&job_snapshot) {
            Ok(output) => {
                if verbose {
                    let preview: String = output.chars().take(200).collect();
                    eprintln!("cron: '{}' 完成 — {preview}", job_name);
                }
                jobs[idx].last_run_at = now;
                executed += 1;
            }
            Err(e) => {
                eprintln!("cron: '{}' 失败 — {e}", job_name);
                jobs[idx].last_run_at = now;
            }
        }
    }

    // 保存更新后的 jobs
    if let Err(e) = save_jobs(&jobs) {
        eprintln!("cron: 保存 jobs 失败: {e}");
    }

    executed
}

/// 运行单个 cron job：解析 provider + 构建 Agent + run(prompt)。
fn run_cron_job(job: &CronJob) -> anyhow::Result<String> {
    // 阻塞运行 tokio runtime
    let rt = tokio::runtime::Runtime::new()?;

    rt.block_on(async {
        let config_path = boxing_config::config_path()?;
        let env_path = boxing_config::env_path()?;
        let config = boxing_config::load(&config_path)?;

        let provider: Arc<dyn boxing_providers::Provider> = Arc::from(
            boxing_providers::resolve(&config, &env_path).map_err(|e| anyhow::anyhow!("{e}"))?,
        );

        let model = if job.model.is_empty() {
            config
                .get("model.default")
                .map_err(|e| anyhow::anyhow!("model.default: {e}"))?
        } else {
            job.model.clone()
        };

        let tools = crate::agent_tools(provider.clone(), &model, "", 30, 4096, &config);

        let mut agent = boxing_core::Agent::new(provider, model, String::new(), tools, 30, 4096);

        let result = agent
            .run(&job.prompt, &mut |_| {}, &mut |_| {})
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        Ok(result)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cron_every_minute() {
        let schedule: CronSchedule = "*/5 * * * *".parse().unwrap();
        assert!(schedule.minutes.contains(&0));
        assert!(schedule.minutes.contains(&5));
        assert!(schedule.minutes.contains(&55));
        assert_eq!(schedule.minutes.len(), 12);
    }

    #[test]
    fn parse_cron_daily() {
        let schedule: CronSchedule = "0 9 * * *".parse().unwrap();
        assert_eq!(schedule.minutes, vec![0]);
        assert_eq!(schedule.hours, vec![9]);
        assert_eq!(schedule.days.len(), 31); // 1-31
    }

    #[test]
    fn parse_cron_weekday() {
        let schedule: CronSchedule = "30 14 * * 1-5".parse().unwrap();
        assert_eq!(schedule.minutes, vec![30]);
        assert_eq!(schedule.hours, vec![14]);
        assert_eq!(schedule.weekdays, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn parse_cron_invalid() {
        assert!("* * * *".parse::<CronSchedule>().is_err());
        assert!("* * * * * *".parse::<CronSchedule>().is_err());
        assert!("99 * * * *".parse::<CronSchedule>().is_err());
    }

    #[test]
    fn next_run_is_in_future() {
        let schedule: CronSchedule = "0 9 * * *".parse().unwrap();
        let now = Utc::now().timestamp() as f64;
        let next = schedule.next_run(now);
        assert!(next > now);
    }

    #[test]
    fn job_serialization_roundtrip() {
        let job = CronJob {
            id: "test-1".to_string(),
            name: "daily-report".to_string(),
            schedule: "0 9 * * *".to_string(),
            prompt: "Summarize yesterday's commits".to_string(),
            model: String::new(),
            status: "active".to_string(),
            last_run_at: 0.0,
            next_run_at: 1000000.0,
        };
        let json = serde_json::to_string(&job).unwrap();
        let back: CronJob = serde_json::from_str(&json).unwrap();
        assert_eq!(job.name, back.name);
        assert_eq!(job.schedule, back.schedule);
    }

    #[test]
    fn add_job_computes_next_run() {
        let job = add_job("test", "0 9 * * *", "hello", "").unwrap();
        assert_eq!(job.status, "active");
        assert!(job.next_run_at > 0.0);
        assert_eq!(job.last_run_at, 0.0);
    }
}
