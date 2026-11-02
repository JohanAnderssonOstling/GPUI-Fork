use anyhow::Result;
use collections::{FxHashMap, FxHashSet};
use sha2::{Digest, Sha256};
use std::{borrow::Cow, ptr};

#[derive(Default)]
pub(crate) struct FontRegistrations {
    by_length: FxHashMap<usize, RegisteredFonts>,
}

enum RegisteredFonts {
    Borrowed(&'static [u8]),
    Hashed(FxHashSet<[u8; 32]>),
}

impl FontRegistrations {
    pub(crate) fn add_fonts(
        &mut self,
        fonts: Vec<Cow<'static, [u8]>>,
        mut register: impl FnMut(Cow<'static, [u8]>) -> Result<()>,
    ) -> Result<()> {
        for bytes in fonts {
            let length = bytes.len();
            if let Some(registered) = self.by_length.get_mut(&length) {
                match registered {
                    RegisteredFonts::Borrowed(previous) => {
                        if let Cow::Borrowed(current) = &bytes
                            && ptr::eq(*previous, *current)
                        {
                            continue;
                        }

                        // A length collision needs content hashes to distinguish fonts.
                        let previous_hash = Sha256::digest(*previous).into();
                        let hash = Sha256::digest(bytes.as_ref()).into();
                        let mut hashes = FxHashSet::from_iter([previous_hash]);
                        if hash != previous_hash {
                            register(bytes)?;
                            hashes.insert(hash);
                        }
                        *registered = RegisteredFonts::Hashed(hashes);
                    }
                    RegisteredFonts::Hashed(hashes) => {
                        let hash = Sha256::digest(bytes.as_ref()).into();
                        if !hashes.contains(&hash) {
                            register(bytes)?;
                            hashes.insert(hash);
                        }
                    }
                }
            } else {
                // Static data stays valid without retaining a copy or reading
                // every font page before its glyphs are needed.
                let registered = match &bytes {
                    Cow::Borrowed(data) => RegisteredFonts::Borrowed(*data),
                    Cow::Owned(_) => {
                        RegisteredFonts::Hashed(FxHashSet::from_iter([Sha256::digest(
                            bytes.as_ref(),
                        )
                        .into()]))
                    }
                };
                register(bytes)?;
                self.by_length.insert(length, registered);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod font_registration_tests {
    use super::*;

    #[test]
    fn borrowed_ui_fonts_and_owned_reader_fonts_share_a_registration() -> Result<()> {
        let mut registrations = FontRegistrations::default();
        let mut installed = Vec::new();
        registrations.add_fonts(vec![Cow::Borrowed(b"shared font")], |font| {
            installed.push(font.into_owned());
            Ok(())
        })?;
        registrations.add_fonts(vec![Cow::Owned(b"shared font".to_vec())], |_| {
            panic!("an existing font must not be registered again")
        })?;
        assert_eq!(installed, [b"shared font".to_vec()]);
        Ok(())
    }

    #[test]
    fn duplicates_in_a_batch_are_skipped_and_distinct_data_is_registered() -> Result<()> {
        let mut registrations = FontRegistrations::default();
        let mut installed = Vec::new();
        registrations.add_fonts(
            vec![
                Cow::Borrowed(b"family A: Latin subset"),
                Cow::Borrowed(b"family A: Latin subset"),
                Cow::Borrowed(b"family A: full coverage"),
                Cow::Borrowed(b"family A: revised outlines"),
            ],
            |font| {
                installed.push(font.into_owned());
                Ok(())
            },
        )?;
        assert_eq!(
            installed,
            [
                b"family A: Latin subset".to_vec(),
                b"family A: full coverage".to_vec(),
                b"family A: revised outlines".to_vec(),
            ]
        );
        Ok(())
    }

    #[test]
    fn failed_registration_can_retry_without_reinstalling_successful_fonts() -> Result<()> {
        let mut registrations = FontRegistrations::default();
        let fonts = vec![
            Cow::Borrowed(b"font A".as_slice()),
            Cow::Borrowed(b"font B"),
        ];
        let mut attempted = Vec::new();
        let error = registrations.add_fonts(fonts.clone(), |font| {
            attempted.push(font.into_owned());
            if attempted.len() == 2 {
                Err(anyhow::anyhow!("platform rejected font B"))
            } else {
                Ok(())
            }
        });
        assert!(error.is_err());
        registrations.add_fonts(fonts, |font| {
            attempted.push(font.into_owned());
            Ok(())
        })?;
        assert_eq!(
            attempted,
            [b"font A".to_vec(), b"font B".to_vec(), b"font B".to_vec()]
        );
        Ok(())
    }

    #[test]
    fn registration_is_scoped_to_each_text_system() -> Result<()> {
        let mut installed = 0;
        for _ in 0..2 {
            FontRegistrations::default().add_fonts(vec![Cow::Borrowed(b"shared font")], |_| {
                installed += 1;
                Ok(())
            })?;
        }
        assert_eq!(installed, 2);
        Ok(())
    }
}
