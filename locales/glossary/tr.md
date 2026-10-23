# Wise Wren – glossary (Turkish, `tr`)

Draft: not yet reviewed by native speakers.

Wren's own strings are written from English with these words. Core strings shared with Vigilant Vole come from
vole-kit and follow [Vigilant Vole's glossary](https://github.com/Ferdinand99/Vigilant-Vole/blob/main/locales/glossary/tr.md).

## Style
- Address: neutral polite imperative (Girin, Seçin, Ekleyin); never "sen" forms. Buttons use the short imperative
  (Kaydet, Sil, İptal).
- Quotation marks: “ ” and ’ as in English. Ellipsis … kept where English has it.
- Sentence case. Respect İ/ı. Month and weekday names are capitalised (Ekim, Salı).
- Suffixes are never attached to `{placeholders}`; sentences are reshaped ("Düzenle: {name}", "“{name}” silinsin mi?").
- Numbers: nouns stay singular after numerals (`{n} sunucu`), so `one` and `other` are identical.
- Kept in English: Docker, Compose, Unraid, `agent.toml`, `WW_SETUP_TOKEN`, `WW_PUBLIC_URL`, `docker run`, JSON, webhook, and other product, protocol and
  technical names.

## Terms

| English | Türkçe | Note |
|---|---|---|
| update (noun) | güncelleme |  |
| update (verb) | güncellemek |  |
| target | hedef | a container or Compose stack Wren updates; VV: “hedef”; container = “kapsayıcı” (VV) |
| agent | ajan | VV: “uzak ajan” |
| server | sunucu |  |
| rollback / roll back | geri alma / geri almak |  |
| snapshot | anlık görüntü |  |
| maintenance window | bakım penceresi | VV |
| notification channel | bildirim kanalı | VV |
| token / agent token | belirteç / ajan belirteci | VV: “API belirteci” |
| revoke | iptal etmek | kit: “İptal et” |
| online / offline | çevrimiçi / çevrimdışı | VV: “Çevrimiçi”; kit: “Çevrimdışı” |
| waiting for the agent | ajan bekleniyor | VV: “İlk bağlantı bekleniyor” |
| overview | genel bakış |  |
| host name | ana bilgisayar adı |  |
| operating system | işletim sistemi | as Kestrel |
| Docker / Compose / Unraid | Docker / Compose / Unraid | unchanged; product names; suffixes with ’ (“Unraid’de”) |
| auto window | otomatik pencere | per-server window in which automatic updates may run |
| scheduled update | zamanlanmış güncelleme | an update approved to run at a chosen time |
| monitor (Vigilant Vole) | monitör | VV: “monitör” |
