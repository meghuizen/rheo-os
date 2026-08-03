# Hoofdstuk 23 - Zonder slot: lock-free en wait-free strategieen

In de vorige hoofdstukken heb je gezien hoe een **lock** (slot) werkt: een draad
die bij gedeelde data wil, pakt het slot, doet zijn werk, en geeft het slot weer
vrij. Dat werkt prima - tot een draad met het slot in zijn hand crasht, of de
processor niet meer krijgt. Dan zit iedereen te wachten. Kan het ook zonder slot?
Ja, maar het is lastiger dan het klinkt.

## Waarom zou je zonder slot willen?

Stel je voor: een druk kruispunt met een stoplicht. Het stoplicht is je lock.
Het werkt goed, maar als het stoplicht kapotgaat, staat alles stil. Niemand kan
meer verder.

Dat is het probleem met locks. Als de draad die het slot vasthoudt wordt
onderbroken (door de scheduler, door een crash, door een bug), staan alle andere
draden stil. In sommige systemen - een database, een netwerkkaart, een
besturingssysteem - is dat onacceptabel.

**Lock-free** en **wait-free** zijn twee manieren om zonder slot te werken. Ze
klinken bijna hetzelfde, maar er is een belangrijk verschil.

## Lock-free: er maakt altijd iemand voortgang

Bij een **lock-free** ontwerp garandeer je: *op elk moment maakt minstens een
draad voortgang*. Het kan zijn dat jouw draad het even niet haalt (omdat een
ander sneller was), maar het systeem als geheel staat nooit stil.

Het gereedschap hiervoor is de **CAS-operatie** (Compare-And-Swap). CAS doet in
een keer drie dingen:

1. Lees de huidige waarde op een adres.
2. Vergelijk die met wat je verwacht.
3. Als het klopt: schrijf de nieuwe waarde. Als het niet klopt: doe niks.

Het mooie: de processor doet dit in een **ondeelbare stap** (atomair). Niemand
kan er tussenin komen.

In pseudocode:

```text
CAS(adres, verwacht, nieuw):
    als *adres == verwacht:
        *adres = nieuw
        geef GELUKT terug
    anders:
        geef MISLUKT terug
```

Als CAS mislukt, betekent dat: iemand anders was sneller. Je leest de nieuwe
waarde opnieuw en probeert het nog een keer. Dat is een **CAS-lus**.

## Een CAS-lus in beeld: de Treiber-stack

De bekendste lock-free datastructuur is de **Treiber-stack**, vernoemd naar zijn
uitvinder. Het is een stapel (stack) waar meerdere draden tegelijk op kunnen
pushen en poppen, zonder lock.

Hoe werkt het? De stapel is een ketting van blokjes, en je houdt een **top**-
aanwijzer bij. Pushen is: een nieuw blokje maken dat naar de oude top wijst, en
dan met CAS de top-aanwijzer verwisselen.

```text
Draad A wil "X" pushen:

Stap 1: Lees top -> [B] -> [C] -> nul

Stap 2: Maak blokje [X], laat X wijzen naar [B]

         [X] -> [B] -> [C] -> nul
          ^
          nieuwe top?

Stap 3: CAS(top, verwacht=[B], nieuw=[X])
         Als top nog steeds [B] is: GELUKT! Top is nu [X].
         Als iemand anders sneller was: MISLUKT. Probeer opnieuw.
```

Het mooie: als CAS mislukt, is er niets kapotgegaan. Een ander heeft gewoon
iets gepusht terwijl jij bezig was. Je leest de nieuwe top, past je blokje
aan, en probeert het opnieuw. Het systeem staat nooit stil.

## Wait-free: iedereen maakt voortgang

**Wait-free** gaat een stap verder: *elke* draad voltooit zijn operatie in een
begrensd aantal stappen, ongeacht wat de andere draden doen. Geen enkele draad
kan eindeloos blijven proberen.

Dat is veel moeilijker dan lock-free. Bij een CAS-lus kan een ongelukkige
draad in theorie eindeloos blijven mislukken als andere draden steeds sneller
zijn. In de praktijk gebeurt dat bijna nooit, maar de garantie is er niet.

Wait-free algoritmen bestaan, maar ze zijn zeldzaam en ingewikkeld. In de
praktijk kiest bijna iedereen voor lock-free en accepteert dat een draad *heel
soms* een extra ronde moet proberen. De meeste datastructuren die je
"lock-free" ziet in echte besturingssystemen en databases, zijn lock-free maar
niet wait-free.

## Het ABA-probleem

Er is een gemene valkuil bij lock-free programmeren die het **ABA-probleem**
heet. Stel je voor:

1. Draad 1 leest de top van de stapel: A.
2. Draad 1 wordt onderbroken.
3. Draad 2 popt A, popt B, pusht A weer terug (misschien met andere inhoud).
4. Draad 1 wordt wakker. Hij ziet: top is nog steeds A. CAS slaagt!

Maar de stapel is *compleet veranderd* achter zijn rug. B is weg, en A wijst
misschien naar iets anders dan eerst. Draad 1 denkt dat alles goed is, terwijl
er data verloren gaat.

Het heet ABA omdat de waarde veranderde van A naar B en dan terug naar A. De
CAS ziet alleen de begin- en eindwaarde, niet wat er tussenin is gebeurd.

### Oplossingen voor ABA

Er zijn twee populaire manieren om ABA te voorkomen:

**1. Een teller erbij (tagged pointer).** Plak een tellerwaarde aan je
aanwijzer. Elke keer dat je de aanwijzer verandert, verhoog je de teller.
Nu is "A met teller 1" iets anders dan "A met teller 2", en CAS ziet het
verschil.

**2. Epoch-based reclamation (tijdperkgebaseerd opruimen).** Je verdeelt de
tijd in "tijdperken". Een draad mag data pas opruimen als alle draden die de
oude data hadden kunnen zien, klaar zijn. Zo kan een adres niet hergebruikt
worden terwijl iemand er nog naar kijkt.

Beide oplossingen zijn ingewikkelder dan een lock. Dat is de prijs die je
betaalt voor het weglaten van het slot.

## Een lock-free MPSC-wachtrij

Een veelgebruikt patroon is een **MPSC-queue** (Multiple Producer, Single
Consumer): meerdere draden stoppen berichten in een wachtrij, en een draad
leest ze eruit. Denk aan meerdere bezorgers die pakketjes in een brievenbus
stoppen, en een bewoner die ze leegt.

Het idee is vergelijkbaar met de Treiber-stack: de schrijvers gebruiken CAS om
hun bericht aan de ketting te hangen. De lezer kan zonder CAS lezen, omdat hij
de enige lezer is (SPSC = Single Producer, Single Consumer zou nog simpeler
zijn).

## Wanneer wel een lock, wanneer lock-free?

**Lock-free is niet altijd sneller!** Dat is misschien de belangrijkste les van
dit hoofdstuk. Lock-free heeft voordelen:

- Geen risico dat een draad het slot vasthoudt en crasht.
- Geen "priority inversion" (een langzame draad blokkeert een snelle).

Maar ook nadelen:

- De code is veel ingewikkelder (en dus moeilijker foutloos te krijgen).
- CAS-lussen verspillen rekenkracht als er veel draden tegelijk proberen.
- Caches werken minder goed als veel processorcores naar dezelfde plek
  schrijven.

**Vuistregel:** gebruik een gewoon slot tenzij je een heel goede reden hebt om
het niet te doen. Die goede redenen zijn:

- Interruptafhandeling (je kunt geen slot pakken vanuit een interrupthandler
  als de onderbroken code datzelfde slot al had).
- Situaties waar een wachtende draad het hele systeem blokkeert.
- Hoge concurrentie op een heel klein stukje data (een teller, een aanwijzer).

In rheo-os zie je allebei: `kernel/src/smp.rs` bevat een `SpinLock` (een echt
slot), terwijl de ringbuffers in `kernel/src/obs/ring.rs` en
`kernel/src/queue/mod.rs` lock-free werken met atomaire operaties.

## Samenvatting

- **Lock-free** garandeert dat het systeem als geheel altijd voortgang maakt.
  Het basisgereedschap is **CAS** (Compare-And-Swap), een atomaire
  vergelijk-en-verwissel-operatie.
- **Wait-free** garandeert dat *elke* draad individueel voortgang maakt. Dat is
  veel moeilijker en zeldzamer.
- De **Treiber-stack** is het schoolvoorbeeld van een lock-free datastructuur:
  push en pop met CAS op de top-aanwijzer.
- Het **ABA-probleem** ontstaat als een waarde verandert en terugverandert
  zonder dat CAS het merkt. Oplossingen: een teller aan de aanwijzer, of
  tijdperkgebaseerd opruimen.
- Lock-free is **niet altijd sneller**. Gebruik een gewoon slot tenzij je een
  sterke reden hebt om het niet te doen.

## Oefeningen

1. Leg in je eigen woorden uit wat het verschil is tussen lock-free en
   wait-free. Geef bij elk een voorbeeld uit het dagelijks leven.
2. Schrijf in pseudocode de stappen op voor "pop" van een Treiber-stack met
   CAS. Wat doe je als CAS mislukt?
3. Bedenk een situatie waarin het ABA-probleem optreedt met een wachtrij in
   plaats van een stapel.
4. Waarom is lock-free niet altijd sneller dan een gewoon slot? Noem twee
   redenen.
5. In rheo-os wordt `SpinLock` gebruikt voor het frame-pool (`kernel/src/mm/`).
   Bedenk waarom daar een slot beter past dan een CAS-lus.
