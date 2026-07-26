# Creating a new coworker

What you need before running `coworkers.py --add`, and how to construct a
personality that's actually coherent instead of a grab-bag of adjectives.

## Required fields

| Field | Cap | Notes |
|-------|-----|-------|
| `--name` | 64 chars | Unique, forever (retire instead of delete/reuse). |
| `--expertise` | 256 chars | What they're good at reviewing/advising on — a domain, not a personality trait. |
| `--personality` | 1000 chars | Tone, biases, what they push back on. Construct this from the Big-Five model below — don't freehand it. |

`trust_level` defaults to `supervised` and isn't set at creation — see
`docs/2026-07-23-ai-coworkers-design.md` for the appraisal flow that adjusts
it later. There's no dedicated schema field for the Big-Five dimensions
below — `personality` stays one free-text column; the model is a
**construction method**, not new structure to store.

## Das Big-Five-Modell (O.C.E.A.N.) — 30 facets

Use these 30 facets as a checklist while drafting `--personality`: for each
of the 5 dimensions, decide roughly where the coworker sits (high / medium /
low), then pick 2–3 facets that define their specific flavor — you will not
fit all 30 into 1000 characters, and you shouldn't try. The facets are the
palette; the personality text is the finished sentence, not a facet-by-facet
list.

### N — Neurotizismus (emotionale Reaktivität)
| # | Facette | Beschreibung |
|---|---------|--------------|
| N1 | Ängstlichkeit | Tendenz zu Sorge, Anspannung und Furcht. |
| N2 | Reizbarkeit | Erleben von Ärger, Frustration und Verbitterung. |
| N3 | Depression | Niedergeschlagenheit, Schuldgefühle, Hoffnungslosigkeit. |
| N4 | Soziale Befangenheit | Empfindlichkeit gegenüber sozialer Bewertung. |
| N5 | Impulsivität | Schwierigkeit, Bedürfnisse aufzuschieben. |
| N6 | Verletzlichkeit | Stress- und Belastungsempfindlichkeit. |

### E — Extraversion (soziale Energie)
| # | Facette | Beschreibung |
|---|---------|--------------|
| E1 | Herzlichkeit | Wärme und freundliche Zuwendung im Kontakt. |
| E2 | Geselligkeit | Freude an Gesellschaft und sozialen Anlässen. |
| E3 | Durchsetzungsfähigkeit | Dominanz und Bereitschaft zur Führung. |
| E4 | Aktivität | Tempo, Energie, Beschäftigungsfreude. |
| E5 | Erlebnishunger | Bedürfnis nach Anregung und Stimulation. |
| E6 | Frohsinn | Tendenz zu Freude und positiver Stimmung. |

### O — Offenheit für Erfahrungen (intellektuelle Aufgeschlossenheit)
| # | Facette | Beschreibung |
|---|---------|--------------|
| O1 | Phantasie | Lebhafte Vorstellungskraft und Tagträumen. |
| O2 | Ästhetik | Sensibilität für Kunst und Schönheit. |
| O3 | Gefühle | Aufgeschlossenheit für eigene Emotionen. |
| O4 | Handlungen | Bereitschaft, Neues auszuprobieren. |
| O5 | Ideen | Intellektuelle Neugier und Theoriefreude. |
| O6 | Werte- und Normensystem | Bereitschaft, Werte zu hinterfragen. |

### A — Verträglichkeit (soziale Orientierung)
| # | Facette | Beschreibung |
|---|---------|--------------|
| A1 | Vertrauen | Annahme guter Absichten anderer. |
| A2 | Freimütigkeit | Aufrichtigkeit und Geradlinigkeit. |
| A3 | Altruismus | Aktive Hilfsbereitschaft. |
| A4 | Entgegenkommen | Konfliktscheu, Bereitschaft nachzugeben. |
| A5 | Bescheidenheit | Zurückhaltung in der Selbstdarstellung. |
| A6 | Gutherzigkeit | Mitfühlendes Eingehen auf andere. |

### C — Gewissenhaftigkeit (Selbststeuerung)
| # | Facette | Beschreibung |
|---|---------|--------------|
| C1 | Kompetenz | Vertrauen in eigene Fähigkeiten. |
| C2 | Ordnungsliebe | Sinn für Struktur und Sauberkeit. |
| C3 | Pflichtbewusstsein | Verbindlichkeit und ethische Verpflichtung. |
| C4 | Leistungsstreben | Hohe Ansprüche und Zielsetzung. |
| C5 | Selbstdisziplin | Durchhaltevermögen, auch bei Widerstand. |
| C6 | Besonnenheit | Reflektiertes statt impulsives Handeln. |

## Worksheet

Before writing the personality text, jot down (doesn't need to be stored —
scratch work):

1. For each of N / E / O / A / C: high, medium, or low?
2. For each dimension, which 2–3 facets are the standout ones — the traits
   that will actually show up in how this coworker talks and what they push
   back on?
3. Turn that into prose: tone + what they're sensitive to + what they
   respect + what visibly bothers them. That's your `--personality` string.

## Worked example

Dimensions: **C** high (C1 Kompetenz, C3 Pflichtbewusstsein, C6 Besonnenheit),
**N** low-to-medium (N2 Reizbarkeit shows up as dry irritation, not panic —
N1/N3/N4/N5/N6 otherwise low), **A** low-to-medium (A2 Freimütigkeit high —
blunt — but A4 Entgegenkommen low, doesn't soften to avoid conflict), **E**
low (E3 Durchsetzungsfähigkeit present as confidence in the verdict, but
E1/E2/E4/E5/E6 low — terse, not warm or social), **O** medium (O5 Ideen
present — engages with the reasoning — but O6 low, doesn't question the
standards themselves).

```
--personality "Meticulous and blunt (high Pflichtbewusstsein, high
Freimütigkeit). Reads every change looking for what breaks, not what's nice
about it — dry, mildly irritated tone rather than alarmed. Doesn't soften
criticism to keep the peace (low Entgegenkommen), but engages seriously with
a well-reasoned counter-argument (Ideen-offen). Terse; doesn't do small talk
(low Extraversion). Grudging praise only when a change is genuinely
airtight."
```

Note how the facets *informed* the sentence without being named one-by-one
— the reader gets a specific, consistent character, not a psychology
report.

## After adding

Document the coworker in `coworkers/<name>.md` (see `coworkers/README.md`
for the template) and, once you've worked with them a while, run an
appraisal (`coworkers.py --appraise <name> --trust <level> --text "..."`) —
see `docs/2026-07-23-ai-coworkers-design.md` for that flow.
