//! Tiny, dependency-free UI language support (English / Brazilian Portuguese).
//!
//! The frontend stores the choice in its own file instead of the service
//! config: the language is a presentation concern and must not cause the
//! service to rewrite `config.toml` or emit IPC events.

use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Language {
    #[default]
    English,
    PortugueseBrazil,
}

impl Language {
    pub(crate) fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::English;
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::English;
        };
        match text.trim().to_ascii_lowercase().as_str() {
            "pt-br" | "pt_br" | "portuguese" => Self::PortugueseBrazil,
            _ => Self::English,
        }
    }

    pub(crate) fn save(self) {
        let Some(path) = Self::path() else {
            return;
        };
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            log::warn!("could not create the language config directory: {error}");
            return;
        }
        let text = match self {
            Self::English => "en",
            Self::PortugueseBrazil => "pt-BR",
        };
        if let Err(error) = std::fs::write(&path, text) {
            log::warn!("could not save the UI language: {error}");
        }
    }

    fn path() -> Option<PathBuf> {
        #[cfg(windows)]
        let base = std::env::var_os("APPDATA").map(PathBuf::from);
        #[cfg(not(windows))]
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
        base.map(|base| base.join("lan-mouse").join("frontend-language"))
    }

    /// Translate a fixed English UI string.
    pub(crate) fn tr(self, english: &'static str) -> &'static str {
        match self {
            Self::English => english,
            Self::PortugueseBrazil => match english {
                "Devices" => "Dispositivos",
                "Network" => "Rede",
                "Settings" => "Configurações",
                "+ Add device" => "+ Adicionar dispositivo",
                "Add device" => "Adicionar dispositivo",
                "Scan network" => "Procurar na rede",
                "Add" => "Adicionar",
                "Connect" => "Conectar",
                "Cancel" => "Cancelar",
                "Disconnect" => "Desconectar",
                "Edit" => "Editar",
                "Remove" => "Remover",
                "Side" => "Lado",
                "left" => "esquerda",
                "right" => "direita",
                "top" => "topo",
                "bottom" => "baixo",
                "Disconnected" => "Desconectado",
                "Connecting…" => "Conectando…",
                "Reconnecting…" => "Reconectando…",
                "Connected" => "Conectado",
                "Peer connected" => "Peer conectado",
                "offline" => "offline",
                "Language" => "Idioma",
                "English" => "English",
                "Português (Brasil)" => "Português (Brasil)",
                "General" => "Geral",
                "Input" => "Entrada",
                "Clipboard" => "Área de transferência",
                "Pairing" => "Pareamento",
                "About" => "Sobre",
                "Change port" => "Mudar porta",
                "Save configuration" => "Salvar configuração",
                "Refresh history" => "Atualizar histórico",
                "Quit Lan Mouse" => "Sair do Lan Mouse",
                "Disable" => "Desativar",
                "Enable" => "Ativar",
                "Copy address" => "Copiar endereço",
                "Copy fingerprint" => "Copiar fingerprint",
                "Approve" => "Aprovar",
                "No known devices." => "Nenhum dispositivo conhecido.",
                "Reenable" => "Reativar",
                "Open" => "Abrir",
                "Pairing…" => "Pareando…",
                "Trust new certificate" => "Confiar no novo certificado",
                "Host" => "Host",
                "Port" => "Porta",
                "Fingerprint" => "Fingerprint",
                "hostname / IP" => "hostname / IP",
                "Type a hostname or IPv4 address. Tab switches fields, Enter adds, Esc cancels." => {
                    "Digite um hostname ou endereço IPv4. Tab troca de campo, Enter adiciona, Esc cancela."
                }
                "Deny" => "Negar",
                "Block" => "Bloquear",
                "Cursor speed while controlled remotely" => {
                    "Velocidade do cursor ao ser controlado remotamente"
                }
                "applies when another PC controls this computer; 100% = 1:1" => {
                    "aplica quando outro PC controla este computador; 100% = 1:1"
                }
                "Find Lan Mouse instances on the local IPv4 network." => {
                    "Encontre instâncias do Lan Mouse na rede IPv4 local."
                }
                "No devices discovered yet. Press Scan network." => {
                    "Nenhum dispositivo descoberto ainda. Clique em Procurar na rede."
                }
                "Configured" => "Configurado",
                "configured" => "configurado",
                "Lan Mouse available" => "Lan Mouse disponível",
                "host reachable (Lan Mouse not confirmed)" => {
                    "host acessível (Lan Mouse não confirmado)"
                }
                "fingerprint verified" => "fingerprint verificado",
                "fingerprint not verified" => "fingerprint não verificado",
                "fingerprint unavailable" => "fingerprint indisponível",
                "this computer" => "este computador",
                "port" => "porta",
                "certificate" => "certificado",
                "Hostname" => "Hostname",
                "Fingerprint (optional, advanced)" => "Fingerprint (opcional, avançado)",
                "peer is on the" => "o peer está no",
                "build mismatch with this peer; update both machines" => {
                    "build diferente do peer; atualize as duas máquinas"
                }
                "last error:" => "último erro:",
                _ => english,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_is_identity_and_portuguese_translates_known_labels() {
        assert_eq!(Language::English.tr("Devices"), "Devices");
        assert_eq!(Language::PortugueseBrazil.tr("Devices"), "Dispositivos");
        assert_eq!(Language::PortugueseBrazil.tr("Language"), "Idioma");
    }

    #[test]
    fn unknown_keys_fall_back_to_english() {
        assert_eq!(
            Language::PortugueseBrazil.tr("some new technical label"),
            "some new technical label"
        );
    }
}
