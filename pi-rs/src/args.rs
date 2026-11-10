// 命令行参数：只解析 maker-core 启动 Pi 时会传的参数（index.ts 里组装 args 的那段）。
// 不认识的参数直接报错，避免静默忽略导致行为和原版不一致。

#[derive(Debug, Default, Clone)]
pub struct Args {
    pub version: bool,
    pub mode: Option<String>,
    pub session_dir: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub tools: Option<Vec<String>>,
    pub extensions: Vec<String>,
    pub skills: Vec<String>,
    pub prompt_templates: Vec<String>,
    pub append_system_prompt: Vec<String>,
    pub system_prompt: Option<String>,
    pub no_extensions: bool,
    pub no_skills: bool,
    pub no_context_files: bool,
    pub no_prompt_templates: bool,
    pub no_approve: bool,
}

pub fn parse(argv: Vec<String>) -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = argv.into_iter();
    while let Some(flag) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} requires a value"));
        match flag.as_str() {
            "--version" | "-v" => a.version = true,
            "--mode" => a.mode = Some(value("--mode")?),
            "--session-dir" => a.session_dir = Some(value("--session-dir")?),
            "--provider" => a.provider = Some(value("--provider")?),
            "--model" => a.model = Some(value("--model")?),
            "--tools" => {
                let v = value("--tools")?;
                a.tools = Some(v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect());
            }
            "--extension" | "-e" => a.extensions.push(value("--extension")?),
            "--skill" => a.skills.push(value("--skill")?),
            "--prompt-template" => a.prompt_templates.push(value("--prompt-template")?),
            "--append-system-prompt" => a.append_system_prompt.push(value("--append-system-prompt")?),
            "--system-prompt" => a.system_prompt = Some(value("--system-prompt")?),
            "--no-extensions" | "-ne" => a.no_extensions = true,
            "--no-skills" => a.no_skills = true,
            "--no-context-files" => a.no_context_files = true,
            "--no-prompt-templates" => a.no_prompt_templates = true,
            "--no-approve" => a.no_approve = true,
            other => return Err(format!("Unknown option: {other}")),
        }
    }
    Ok(a)
}
