use crate::pins::PinId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Show,
    Region,
    Fullscreen,
    FullscreenSave,
    NewPin,
    ShowPin(PinId),
    ClosePins,
    OpenConfig,
    ReloadConfig,
    ToggleAutostart,
    Exit,
}

const EXTERNAL: [(Command, &str); 6] = [
    (Command::Show, "show"),
    (Command::Region, "capture-region"),
    (Command::Fullscreen, "capture-fullscreen"),
    (Command::FullscreenSave, "capture-fullscreen-save"),
    (Command::ReloadConfig, "reload-config"),
    (Command::Exit, "exit"),
];

impl Command {
    pub(crate) fn from_ipc(name: &str) -> Option<Self> {
        EXTERNAL
            .into_iter()
            .find_map(|(command, wire)| (wire == name).then_some(command))
    }

    pub(crate) fn ipc_name(self) -> Option<&'static str> {
        EXTERNAL
            .into_iter()
            .find_map(|(command, wire)| (command == self).then_some(wire))
    }
}
