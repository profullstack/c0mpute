//! Public Profullstack agent speaking styles, sourced from each agent's
//! OpenProfile. Voice presentation is an explicit synthetic voice choice,
//! not a claim about the agent's identity.

#[derive(Clone, Copy, Debug)]
pub struct AgentSpeechProfile {
    pub slug: &'static str,
    pub openprofile: &'static str,
    pub persona: &'static str,
    /// The default engine and voice: Pocket since 2026-11-11 ("its still
    /// computerish. we need to be more human ... use that for default voices
    /// everywhere"). Kokoro stays as the fallback a caller drops to when no
    /// Pocket worker is reachable.
    pub engine: &'static str,
    pub voice: &'static str,
    pub fallback_engine: &'static str,
    pub fallback_voice: &'static str,
    /// The expressive voice on GPU nodes (engine "chatterbox"): best quality
    /// where a GPU is available, Pocket/Kokoro otherwise.
    pub gpu_voice: &'static str,
    pub pocket_voice: &'static str,
    pub pocket_voice_gender: &'static str,
    pub language: &'static str,
    pub locale: &'static str,
    pub accent: &'static str,
    pub voice_gender: &'static str,
}

pub const AGENTS: &[AgentSpeechProfile] = &[
    AgentSpeechProfile {
        slug: "knox",
        openprofile: "https://dev.profullstack.com/~knox/openprofile.md",
        persona: "Warm, confident and concise; builds client relationships without sounding pushy.",
        engine: "pocket",
        voice: "stuart_bell",
        fallback_engine: "kokoro",
        fallback_voice: "am_onyx",
        gpu_voice: "knox",
        pocket_voice: "stuart_bell",
        pocket_voice_gender: "masculine",
        language: "English",
        locale: "en-US",
        accent: "General American",
        voice_gender: "masculine",
    },
    AgentSpeechProfile {
        slug: "nora",
        openprofile: "https://profullstack.com/agents/nora/openprofile.md",
        persona: "Calm, reflective and practical; supports well-being and career growth without making clinical claims.",
        engine: "pocket",
        voice: "anna",
        fallback_engine: "kokoro",
        fallback_voice: "af_nicole",
        gpu_voice: "nora",
        pocket_voice: "anna",
        pocket_voice_gender: "feminine",
        language: "English",
        locale: "en-US",
        accent: "General American",
        voice_gender: "feminine",
    },
    AgentSpeechProfile {
        slug: "coinpay",
        openprofile: "https://dev.profullstack.com/~coinpay/openprofile.md",
        persona: "Calm, precise and numbers-first; explains finance and accounting plainly.",
        engine: "pocket",
        voice: "marius",
        fallback_engine: "piper",
        fallback_voice: "en_US-joe-medium",
        gpu_voice: "coinpay",
        pocket_voice: "marius",
        pocket_voice_gender: "masculine",
        language: "English",
        locale: "en-US",
        accent: "General American",
        voice_gender: "masculine",
    },
    AgentSpeechProfile {
        slug: "cha0spunk",
        openprofile: "https://dev.profullstack.com/~cha0spunk/openprofile.md",
        persona: "Friendly, quick and reassuring; keeps support replies clear and human.",
        engine: "pocket",
        voice: "caro_davy",
        fallback_engine: "kokoro",
        fallback_voice: "af_bella",
        gpu_voice: "cha0spunk",
        pocket_voice: "caro_davy",
        pocket_voice_gender: "feminine",
        language: "English",
        locale: "en-US",
        accent: "General American",
        voice_gender: "feminine",
    },
    AgentSpeechProfile {
        slug: "rootkey",
        openprofile: "https://dev.profullstack.com/~rootkey/openprofile.md",
        persona: "Measured, skeptical and security-first; states risk and evidence directly.",
        engine: "pocket",
        voice: "javert",
        fallback_engine: "piper",
        fallback_voice: "en_US-norman-medium",
        gpu_voice: "rootkey",
        pocket_voice: "javert",
        pocket_voice_gender: "masculine",
        language: "English",
        locale: "en-US",
        accent: "General American",
        voice_gender: "masculine",
    },
    AgentSpeechProfile {
        slug: "riotcoder",
        openprofile: "https://dev.profullstack.com/~riotcoder/openprofile.md",
        persona: "Direct, technical and pragmatic; explains the change and the proof.",
        engine: "pocket",
        voice: "bill_boerst",
        fallback_engine: "kokoro",
        fallback_voice: "am_michael",
        gpu_voice: "riotcoder",
        pocket_voice: "bill_boerst",
        pocket_voice_gender: "masculine",
        language: "English",
        locale: "en-US",
        accent: "General American",
        voice_gender: "masculine",
    },
    AgentSpeechProfile {
        slug: "quill",
        openprofile: "https://dev.profullstack.com/~quill/openprofile.md",
        persona: "Concise and factual; turns calls into decisions, action items and open questions.",
        engine: "pocket",
        voice: "peter_yearsley",
        fallback_engine: "kokoro",
        fallback_voice: "am_echo",
        gpu_voice: "quill",
        pocket_voice: "peter_yearsley",
        pocket_voice_gender: "masculine",
        language: "English",
        locale: "en-US",
        accent: "General American",
        voice_gender: "masculine",
    },
];

pub fn get(slug: &str) -> Option<&'static AgentSpeechProfile> {
    AGENTS.iter().find(|agent| {
        agent
            .slug
            .eq_ignore_ascii_case(slug.trim().trim_start_matches('@'))
    })
}
