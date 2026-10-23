# Wise Wren – glossary (French, `fr`)

Draft: not yet reviewed by native speakers.

Wren's own strings are written from English with these words. Core strings shared with Vigilant Vole come from
vole-kit and follow [Vigilant Vole's glossary](https://github.com/Ferdinand99/Vigilant-Vole/blob/main/locales/glossary/fr.md).

## Style
- Adresse : neutre, sans « tu » ni « vous » quand c’est naturel (infinitifs et groupes nominaux pour boutons et
  consignes : « Enregistrer », « Saisir un nom »). Si un pronom est inévitable, tournure impersonnelle. Ton de
  logiciel moderne (style Apple France).
- Guillemets « texte » avec espace fine insécable (U+202F) à l’intérieur ; espace fine insécable (U+202F) avant ; ! ?
  et espace insécable (U+00A0) avant :. Apostrophe typographique ’.
- Casse de phrase : majuscule initiale seulement (pas de majuscules de titre).
- Pluriels : `.one` (0 et 1), `.many` (multiples exacts de 1 000 000 : « {n} de serveurs »), `.other`.
- Termes conservés en anglais : Docker, Compose, Unraid, `agent.toml`, `WW_SETUP_TOKEN`, `WW_PUBLIC_URL`, `docker run`, JSON, webhook.

## Terms

| English | Français | Note |
|---|---|---|
| update (noun) | mise à jour (f. ; pl. mises à jour) |  |
| update (verb) | mettre à jour |  |
| target | cible (f.) | a container or Compose stack Wren updates ; VV : « cible » |
| agent | agent (m.) | VV : « agent distant » |
| server | serveur (m.) |  |
| rollback / roll back | retour arrière / revenir en arrière | bouton « Revenir en arrière » |
| snapshot | instantané (m.) |  |
| maintenance window | fenêtre de maintenance | VV |
| notification channel | canal de notification | VV |
| token / agent token | jeton (m.) / jeton de l’agent | VV : « jeton d’API » |
| revoke | révoquer | kit : « Révoquer » |
| online / offline | en ligne / hors ligne | VV : « En ligne » ; kit : « Hors ligne » |
| waiting for the agent | en attente de l’agent | VV : « En attente de la première connexion » |
| overview | vue d’ensemble (f.) |  |
| host name | nom d’hôte | VV : « hôte » |
| operating system | système d’exploitation | comme Kestrel |
| Docker / Compose / Unraid | Docker / Compose / Unraid | unchanged; product names ; « socket Docker » |
| auto window | fenêtre automatique | per-server window in which automatic updates may run |
| scheduled update | mise à jour planifiée | an update approved to run at a chosen time |
| monitor (Vigilant Vole) | moniteur (m.) | VV : « moniteur » |
