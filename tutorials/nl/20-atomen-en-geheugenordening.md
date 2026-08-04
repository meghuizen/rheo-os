# Hoofdstuk 20 - Atomen en geheugenordening

Stel je voor: twee kassamedewerkers in een winkel willen allebei de
voorraadteller bijwerken. Allebei lezen ze "42 stuks", allebei tellen ze
er 1 bij op, en allebei schrijven ze "43" terug. Maar er zijn twee stuks
verkocht, dus de teller had 44 moeten zijn. Eentje is verloren gegaan.

Precies dit probleem heb je als twee processorcores tegelijk dezelfde
variabele aanpassen. In dit hoofdstuk leer je het gereedschap om dat
veilig te doen: **atomaire operaties** en **geheugenordening**.

## Wat is een atomaire operatie?

Het woord "atomair" komt van het Griekse woord voor "ondeelbaar". Een
**atomaire operatie** is een bewerking die niemand halverwege kan zien.
Het is alles of niets: de bewerking is helemaal gedaan, of helemaal niet.
Er is geen tussenstap die een ander kan waarnemen.

Vergelijk het met een draaideurtje: je kunt erdoor of niet, maar je kunt
niet halverwege blijven steken terwijl iemand anders er ook doorheen wil.

## Waarom heb je ze nodig?

Kijk naar deze code (in Rust-achtige taal):

```text
  count = count + 1;
```

Dat lijkt een stap, maar voor de processor zijn het er drie:

```text
  1. Lees de huidige waarde van count uit het geheugen.
  2. Tel er 1 bij op.
  3. Schrijf het resultaat terug naar het geheugen.
```

Als twee cores dit tegelijk doen:

```text
  Core 0                    Core 1
  ------                    ------
  leest count = 42          leest count = 42
  berekent 42 + 1 = 43      berekent 42 + 1 = 43
  schrijft count = 43       schrijft count = 43

  Resultaat: count = 43 (maar we wilden 44!)
```

De update van core 1 is verloren gegaan. Dit heet een **lost update** of
een **race condition** (een wedstrijdfout).

## De drie belangrijkste atomaire operaties

### 1. Atomaire load en store

Een **atomaire load** leest een waarde in een keer, zodat je nooit een
"half geschreven" getal ziet. Een **atomaire store** schrijft in een keer.

### 2. Fetch-and-add

**Fetch-and-add** leest de oude waarde, telt er iets bij op, en schrijft
het resultaat terug - alles in een ondeelbare stap. Geen andere core kan
ertussen komen.

```text
  fetch_add(&count, 1)  -->  leest 42, schrijft 43, geeft 42 terug
```

Als twee cores dit tegelijk doen, gaat het goed:

```text
  Core 0: fetch_add --> leest 42, schrijft 43
  Core 1: fetch_add --> leest 43, schrijft 44

  Resultaat: count = 44 (correct!)
```

### 3. Compare-and-swap (CAS)

**Compare-and-swap** (vergelijk-en-verwissel, afgekort **CAS**) is de
krachtigste atomaire operatie. Hij zegt: "als de waarde nu X is, verander
hem dan naar Y. Zo niet, doe niets."

```text
  compare_and_swap(&count, verwacht=42, nieuw=43)

  Als count == 42: schrijf 43, geef "gelukt" terug.
  Als count != 42: doe niets, geef "mislukt" terug.
```

CAS is de bouwsteen van bijna alle lock-vrije datastructuren. Als het
mislukt, probeer je het gewoon opnieuw met de nieuwe waarde.

## Atomaire operaties in Rust

In Rust gebruik je `AtomicUsize`, `AtomicBool` en vrienden uit
`core::sync::atomic`:

```text
  use core::sync::atomic::{AtomicUsize, Ordering};

  static COUNT: AtomicUsize = AtomicUsize::new(0);

  // Atomair ophogen:
  COUNT.fetch_add(1, Ordering::Relaxed);

  // Compare-and-swap:
  COUNT.compare_exchange(42, 43, Ordering::SeqCst, Ordering::SeqCst);
```

Dat `Ordering`-argument brengt ons bij het volgende onderwerp.

## Geheugenordening: waarom de volgorde ertoe doet

Hier wordt het verrassend. Je zou denken dat de processor je instructies
keurig op volgorde uitvoert. Maar dat is **niet zo**. Zowel de compiler
als de processor mogen instructies **door elkaar husselen**, zolang het
resultaat er voor *een enkele* core hetzelfde uitziet.

Waarom? Snelheid. De processor kan sneller werken als hij een volgende
instructie alvast begint terwijl de vorige nog bezig is.

Maar als twee cores samenwerken, kan die herschikking problemen geven:

```text
  Core 0 schrijft:              Core 1 leest:
  ----------------              ---------------
  data = 42;                    if (klaar == true) {
  klaar = true;                     lees data;  // misschien NIET 42!
                                }
```

Core 0 schrijft eerst de data en dan de vlag. Maar de processor mag die
twee schrijfopdrachten omdraaien. Core 1 ziet dan `klaar == true` maar
leest de oude, verkeerde data. Dat is een **geheugenordeningsbug**.

```text
  Zonder barriere:

  Core 0 geheugen:    [data=???] [klaar=true]    (omgedraaid!)
  Core 1 ziet:        klaar is true, maar data is oud

  Met barriere:

  Core 0 geheugen:    [data=42] ---barricade--- [klaar=true]
  Core 1 ziet:        klaar is true, en data is 42 (correct)
```

## De vier volgordes in Rust

Rust biedt vier niveaus van ordening, van los naar streng:

1. **Relaxed** - Alleen de atomaire eigenschap (ondeelbaar), geen volgorde.
   Snel, maar je mag er geen "vlaggen" mee zetten. Goed voor tellers die
   niets signaleren.

2. **Acquire** - Gebruik bij het *lezen*. Zegt: "alles wat ik na deze
   leesoperatie doe, mag niet naar voren worden gehaald." Je "pakt" de
   data op.

3. **Release** - Gebruik bij het *schrijven*. Zegt: "alles wat ik voor
   deze schrijfoperatie deed, moet eerst af zijn." Je "laat" de data los
   voor anderen.

4. **SeqCst** (Sequentially Consistent) - De strengste: alle cores zien
   alle operaties in dezelfde volgorde. Makkelijkst om over na te denken,
   maar het langzaamst.

Het Acquire/Release-paar is het meest gebruikte patroon: de schrijver
gebruikt Release ("ik ben klaar"), de lezer gebruikt Acquire ("ik pak het
op"). Samen garanderen ze de juiste volgorde.

## Geheugenbarrieres (fences)

Een **geheugenbarriere** (ook wel **fence** genoemd) is een instructie die
zegt: "alles hiervoor moet klaar zijn voordat je verdergaat." Het is als
een slagboom op de snelweg: het verkeer van voor de slagboom moet eerst
door voordat het verkeer erna mag.

In Rust:

```text
  use core::sync::atomic::fence;

  fence(Ordering::Release);  // alles hiervoor is zichtbaar voor anderen
```

In de praktijk gebruik je liever de ordening op de atomaire operatie zelf
(het `Ordering`-argument) dan een losse fence. Dat is preciezer en soms
sneller.

## Samenvatting

- Een **atomaire operatie** is ondeelbaar: niemand ziet hem halverwege.
- **Fetch-and-add** en **compare-and-swap (CAS)** zijn de twee belangrijkste
  atomaire operaties.
- Zonder atomaire operaties kun je updates verliezen als twee cores
  dezelfde variabele aanpassen (**lost update**).
- De processor en de compiler mogen instructies **herschikken**. Dat kan
  fouten geven als twee cores samenwerken.
- **Ordening** (Relaxed, Acquire, Release, SeqCst) bepaalt hoeveel
  herschikking is toegestaan.
- **Geheugenbarrieres** (fences) dwingen een volgorde af.

## Oefeningen

1. Twee cores doen allebei `count = count + 1` zonder atomaire operatie.
   `count` begint op 0. Welke waarden kan `count` aan het eind hebben?
2. Leg in je eigen woorden uit wat compare-and-swap doet.
3. Waarom is `Relaxed` niet genoeg als je een vlag wilt zetten om aan een
   andere core te signaleren dat data klaar is?
4. Bedenk een situatie uit het dagelijks leven die lijkt op een race
   condition.
5. In rheo-os is `SpinLock` gebouwd op atomaire operaties. Waarom kan een
   gewone `bool` daar niet voor worden gebruikt?

Door naar [hoofdstuk 21](21-meerdere-processoren.md): hoe een
besturingssysteem meerdere processorcores tegelijk aanstuurt.
