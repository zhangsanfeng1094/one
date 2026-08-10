//! Rename `LegacyWidget` → `CanonicalWidget` everywhere in THIS file only.

pub struct LegacyWidget {
    pub id: u32,
}

impl LegacyWidget {
    pub fn new(id: u32) -> Self {
        LegacyWidget { id }
    }

    pub fn bump(self) -> LegacyWidget {
        LegacyWidget { id: self.id + 1 }
    }

    pub fn label(&self) -> String {
        format!("LegacyWidget#{}", self.id)
    }
}

pub fn make_pair() -> (LegacyWidget, LegacyWidget) {
    (LegacyWidget::new(1), LegacyWidget::new(2))
}

pub fn describe(w: &LegacyWidget) -> String {
    w.label()
}
