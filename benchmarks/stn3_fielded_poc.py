#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""STN3 4.6 fielded-terms latency GATE harness (Item 1).

Locked synthetic corpus, dump-segments + breakdown dictionary bytes, search
timing, and the mandatory/advisory decision table. Item 2 executes the 20k×20
three-system protocol; this file does not install the extension.

Subcommands:

  generate   write or hash the locked CSV (id,title,body, QUOTE_ALL, LF)
  run        load/index/search one system using PG* (writes a result fragment)
  merge      combine three fragments, apply gates, write the POC JSON
"""
from __future__ import annotations

import argparse
import csv
import hashlib
import io
import json
import math
import os
from pathlib import Path
import random
import re
import struct
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_QUERIES = ROOT / "docs" / "benchmarks" / "stn3-fielded-poc-queries.txt"
DEFAULT_JSON = ROOT / "docs" / "benchmarks" / "stn3-fielded-poc.json"
DUMP_SEGMENTS = ROOT / "script" / "dump-segments.py"

ROWS = 20_000
SEED = 20260930
ZIPF_S = 1.15
TITLE_TOKENS = (6, 16)
BODY_TOKENS = (40, 240)
TIMED_RUNS = 20
WARMUP_RUNS = 1
SEARCH_LIMIT = 10
MANDATORY_THRESHOLD = 1.8
ADVISORY_P50_THRESHOLD = 1.3
V040_PIN = "7ab511b"
SYSTEMS = ("stn3_fielded", "stn3_single", "v040")
TIMED_SQL = (
    "SELECT d.id FROM documents d "
    "JOIN stannum.search('idx'::regclass, %s, 10) AS s ON d.ctid = s.ctid"
)
INDEX_SQL = {
    "stn3_fielded": "CREATE INDEX idx ON documents USING stannum (title, body)",
    "stn3_single": "CREATE INDEX idx ON documents USING stannum (concat)",
    "v040": "CREATE INDEX idx ON documents USING stannum (title, body)",
}
STNF_MAGIC = b"STNF"
STNF_VERSION = 1
STNF_VERSION_V1 = 1
STNF_VERSION_V2 = 2
STNF_PREFIX_LEN = 13
STNF_PREFIX_LEN_V1 = 13
STNF_PREFIX_LEN_V2 = 9
BREAKDOWN_DICTIONARY = re.compile(r"^\s+dictionary\s+(\d+)\b", re.M)

VOCAB = tuple("""
the of and to a in is it for on are with as his they be at this from or one had by word
but not what all were we when your can said there use an each which she do how their if
will up other about out many then them these so some her would make like him into time
has look two more write go see number no way could people my than first water been call
who oil its now find long down day did get come made may part also back after where most
through much before good new our me very just because any those here using such only
even own over still must between never same another while last should around every few
both under again place well large history telescope war astronomy computer science
united states python database index query search document title body field token segment
merge score rank phrase prefix filter count vacuum buffer page table column engine
storage layout dictionary posting ordinal payload length class bitmap skip server client
latency throughput memory cache pool lock snapshot transaction analyze reindex create
drop alter select insert update delete join group order limit offset having distinct
union except postgres stannum tinql tokenizer unicode jieba folding accent grapheme
position gap saturation normalization weight boost stop common medium rare conjunction
disjunction nested boolean clause term stack exchange wikipedia question answer comment
user badge vote tag revision post excerpt view favorite code function method module
package crate rust java script shell json yaml csv utf eight little endian magic version
header trailer sidecar envelope meta directory pending generation run chain block
special identity spec analysis stamp aal aalii aam aani aardvark aardwolf aaron aaronic
aaronical aaronite aaronitic aaru aba ababdeh ababua abac abaca abacate abacay abacinate
abaciscus abacist aback abactinal abaction abactor abaculus abacus abadite abaff abaft
abaisance abaiser abaissed abalienate abalone abama abampere abandon abandoned abandonee
abandoner abanic abantes abaptiston abarambo abaris abas abase abased abasedly
abasedness abasement abaser abasgi abash abashed abashedly abashless abashment abasia
abasic abask abassin abatable abate abatement abater abatis abatised abaton abator
abattoir abatua abature abave abaxial abaxile abaze abb abba abbacomes abbacy abbadide
abbas abbasi abbassi abbasside abbatial abbatical abbess abbey abbeystede abbie abbot
abbotcy abbotship abbreviate abby abcoulomb abdal abdat abderian abderite abdest
abdicable abdicant abdicate abdication abdicative abdicator abdiel abditive abditory
abdomen abdominal abdominous abduce abducens abducent abduct abduction abductor abe
abeam abear abearance abecedary abed abeigh abel abele abelia abelian abelicea abelite
abelmosk abelonian abeltree abenteric aberdeen aberdevine aberdonian aberia aberrance
aberrancy aberrant aberrate aberration aberrator abet abetment abettal abettor abey
abeyance abeyancy abeyant abfarad abhenry abhiseka abhor abhorrence abhorrency abhorrent
abhorrer abhorrible abhorring abhorson abidal abidance abide abider abidi abiding
abidingly abie abies abietate abietene abietic abietin abietineae abietinic abiezer
abigail abigeat abigeus abilao ability abilla abilo abiogenist abiogenous abiogeny
abiology abiosis abiotic abiotrophy abipon abir abirritant abirritate abiston abitibi
abiuret abject abjection abjective abjectly abjectness abjoint abjudge abjudicate
abjunction abjunctive abjuration abjuratory abjure abjurement abjurer abkar abkari
abkhas abkhasian ablach ablactate ablare ablastemic ablastous ablate ablation ablatival
ablative ablator ablaut ablaze able ableeze ablegate ableness ablepharia ablepharon
ablepharus ablepsia ableptical abler ablest ablins abloom ablow ablude abluent ablush
ablution abluvion ably abmho abnaki abnegate abnegation abnegative abnegator abner
abnerval abnet abneural abnormal abnormally abnormity abnormous abo aboard abobra abode
abodement abody abohm aboil abolish abolisher abolition abolla aboma abomasum abomasus
abominable abominably abominate abominator abomine abongo aboon aborad aboral aborally
abord aboriginal aborigine abort aborted aborticide abortient abortin abortion
abortional abortive abortively abortus abound abounder abounding abouts above aboveboard
abovedeck aboveproof abox abrachia abradant abrade abrader abraham abrahamic abrahamite
abraid abram abramis abranchial abranchian abrasax abrase abrash abrasion abrasive
abrastol abraum abraxas abreact abreaction abreast abrenounce abret abrico abridge
abridged abridgedly abridger abridgment abrim abrin abristle abroach abroad abrocoma
abrocome abrogable abrogate abrogation abrogative abrogator abroma abronia abrook
abrotanum abrotine abrupt abruptedly abruption abruptly abruptness abrus absalom
absampere absaroka absarokite abscess abscessed abscession abscind abscise abscision
absciss abscissa abscissae abscisse abscission absconce abscond absconded absconder
absconsa abscoulomb absence absent absentee absenter absently absentment absentness
absfarad abshenry absi absinthe absinthial absinthian absinthic absinthin absinthine
absinthism absinthium absinthol absit absmho absohm absolute absolutely absolution
absolutism absolutist absolutive absolutize absolutory absolvable absolve absolvent
absolver absolvitor absonant absonous absorb absorbable absorbed absorbedly absorbency
absorbent absorber absorbing absorpt absorption absorptive abstain abstainer abstemious
abstention absterge abstergent abstersion abstersive abstinence abstinency abstinent
abstract abstracted abstracter abstractly abstractor abstrahent abstricted abstruse
abstrusely abstrusion abstrusity absume absumption absurd absurdity absurdly absurdness
absvolt absyrtus abterminal abthain abthainrie abthainry abthanage abu abucco abulia
abulic abulomania abuna abundance abundancy abundant abundantia abundantly abura
aburabozu aburban aburst aburton abusable abuse abusedly abusee abuseful abusefully
abuser abusion abusious abusive abusively abut abuta abutilon abutment abuttal abutter
abutting abuzz abvolt abwab aby abysm abysmal abysmally abyss abyssal abyssinian
abyssolith acacetin acacia acacian acaciin acacin academe academial academian academic
academical academism academist academite academize academus academy acadia acadialite
acadian acadie acaena acajou acaleph acalepha acalephae acalephan acalephoid acalycal
acalycine acalypha acamar acampsia acana acanaceous acanonical acanth acantha acanthad
acantharia acanthia acanthial acanthin acanthine acanthion acanthite acanthodea
acanthodei acanthodes acanthodii acanthoid acanthoma acanthon acanthopod acanthosis
acanthous acanthurus acanthus acapnia acapnial acapsular acapu acapulco acara acarapis
acardia acardiac acari acarian acariasis acaricidal acaricide acarid acarida acaridea
acaridean acariform acarina acarine acarinosis acaroid acarol acarology acarotoxic
acarpelous acarpous acarus acastus acatalepsy acataposis acate acatery acatharsia
acatharsy acatholic acaudal acaudate acauline acaulose acaulous acca accede accedence
acceder accelerant accelerate accend accendible accension accensor accentless accentor
accentual accentuate accentus accept acceptable acceptably acceptance acceptancy
acceptant accepted acceptedly accepter acception acceptive acceptor acceptress accerse
accersitor access accessary accessible accessibly accession accessive accessless
accessory accidence accidency accident accidental accidented accidently accidia accidie
accinge accipient accipiter accipitral accipitres accismus accite acclaim acclaimer
acclamator acclimate acclinal acclinate acclivity acclivous accloy accoast accoil
accolade accoladed accolated accolent accolle accompany accomplice accomplish accompt
accord accordable accordance accordancy accordant accorder according accordion accost
accostable accosted accouche accoucheur account accountant accounting accouple accouter
accoy accredit accredited accresce accrescent accretal accrete accretion accretive
accroach accroides accrual accrue accruement accruer accubation accubitum accubitus
accultural accumbency accumbent accumber accumulate accuracy accurate accurately accurse
accursed accursedly accusable accusably accusal accusant accusation accusative
accusatory accusatrix accuse accused accuser accusingly accusive accustom accustomed ace
acecaffine aceconitic acedia acediamine acediast acedy aceldama acemetae acemetic
acentric acentrous aceologic aceology acephal acephala acephalan acephali acephalia
acephalina acephaline acephalism acephalist acephalite acephalous acephalus acer
aceraceae aceraceous acerae acerata acerate acerates acerathere aceratosis acerb acerbas
acerbate acerbic acerbity acerdol acerin acerose acerous acerra acertannin acervate
acervately acervation acervative acervose acervuline acervulus acescence acescency
acescent aceship acesodyne acestes acetabular acetabulum acetacetic acetal acetalize
acetamide acetamidin acetamido acetaminol acetanilid acetanion acetannin acetarious
acetarsone acetate acetated acetation acetenyl acetic acetifier acetify acetimeter
acetimetry acetin acetize acetoin acetol acetolysis acetolytic acetometer acetometry
acetonate acetone acetonemia acetonemic acetonic acetonize acetonuria acetonyl
acetopyrin acetose acetosity acetous acetoxime acetoxyl acetract acetum aceturic acetyl
acetylate acetylator acetylene acetylenic acetylenyl acetylic acetylide acetylize
acetylizer acetylurea ach achaean achaemenid achaenodon achaeta achaetous achage achagua
achakzai achalasia achamoth achango achar achate achates achatina ache acheilia
acheilous acheiria acheirous acheirus achen achene achenial achenium achenocarp
achenodium acher achernar acheronian acherontic achete achetidae acheulean acheweed
achievable achieve achiever achigan achilary achill achillea achillean achilleid
achilleine achillize achime achimenes achinese aching achingly achira achitophel
achmetha acholia acholic acholoe acholous acholuria acholuric achomawi achondrite achor
achordal achordata achordate achorion achras achree achroacyte achroite achroma
achromasia achromat achromate achromatic achromatin achromia achromic achromous
achronical achroous achropsia achtel achuas achy achylia achylous achymia achymous
achyrodes acicula acicular acicularly aciculate aciculated aciculum acid acidaspis
acidemia acider acidic acidifiant acidific acidifier acidify acidimeter acidimetry
acidite acidity acidize acidly acidness acidoid acidology acidometer acidometry
acidophile acidosis acidotic acidproof acidulate acidulent acidulous aciduric acidyl
acier acierage acieral acierate acieration aciform aciliate aciliated acilius acinaceous
acinaces acinar acinarious acinary acineta acinetae acinetan acinetaria acinetic
acinetina acinetinan acinic aciniform acinose acinous acinus acipenser acis aciurgy
acker ackey ackman acknow aclastic acle acleidian acleistous aclemon aclidian aclinal
aclinic acloud aclys acmaea acmaeidae acmatic acme acmic acmispon acmite acne acneform
acneiform acnemia acnida acnodal acnode acock acockbill acocotl acoela acoelomata
acoelomate acoelomi acoelomous acoelous acoemetae acoemeti acoemetic acoin acoine
acolapissa acold acolhua acolhuan acologic acology acolous acoluthic acolyte acolythate
acoma acomia acomous aconative acondylose acondylous acone aconic aconin aconine
aconital aconite aconitia aconitic aconitin aconitine aconitum acontias acontium
acontius aconuresis acopic acopon acopyrin acopyrine acor acorea acoria acorn acorned
acorus acosmic acosmism acosmist acosmistic acotyledon acouasm acouchi acouchy acoumeter
acoumetry acouometer acoupa acousmata acousmatic acoustic acoustical acousticon
acoustics acquaint acquainted acquest acquiesce acquiescer acquirable acquire acquired
acquirenda acquirer acquisible acquisite acquisited acquisitor acquisitum acquist acquit
acquitment acquittal acquitter acrab acracy acraein acraeinae acrania acranial acraniate
acrasia acrasiales acrasida acrasieae acraspeda acratia acrawl acraze acre acreable
acreage acreak acream acred acredula acreman acrestaff acrid acridan acridian acridic
acrididae acridiidae acridine acridinic acridinium acridity acridium acridly acridness
acridone acridonium acridyl acriflavin acrimony acrinyl acrisia acrisius acrita acritan
acrite acritical acritol acroa acroama acroamatic acroataxia acroatic acrobacy acrobat
acrobates acrobatic acrobatics acrobatism acroblast acrobryous acrocarpi acrocera
acrocomia acrocyst acrodont acrodrome acrodus acrodynia acrogamous acrogamy acrogen
acrogenic acrogenous acrography acrogynae acrogynous acrolein acrolith acrolithan
acrolithic acrologic acrologism acrologue acrology acromania acromegaly acrometer
acromial acromicria acromion acromyodi acromyodic acron acronical acronyc acronych
acronycta acronym acronymic acronymize acronymous acronyx acrook acropathy acropetal
acrophobia acrophonic acrophony acropodium acropoleis acropolis acropora acrorhagus
acrosarc acrosarcum acroscopic acrose acrosome acrospire acrospore across acrostic
acrostical acroterial acroteric acroterium acrotic acrotism acrotomous acrotreta acrux
acrydium acryl acrylate acrylic acrylyl act acta actability actable actaea actaeaceae
actaeon actiad actian actifier actify actin actinal actinally actine acting actinia
actinian actiniaria actinic actinidia actiniform actinine actinism actinistia actinium
actinocarp actinogram actinoid actinoida actinoidea actinolite actinology actinomere
actinon actinonema actinopoda actinosoma actinosome actinost actinozoa actinozoal
actinozoan actinozoon actinula action actionable actionably actional actionary actioner
actionize actionless actipylea actium activable activate activation activator active
actively activeness activin activism activist activital activity activize actless
actomyosin acton actor actorship actress acts actu actual actualism actualist actuality
actualize actually actualness actuarial actuarian actuary actuation actuator acture
acturience actutate acuan acuate acuation acubens acuclosure acuductor acuity aculea
aculeata aculeate aculeated aculeiform aculeolate aculeolus aculeus acumen acuminate
acuminose acuminous acupress acurative acushla acutate acute acutely acuteness acutiator
acutish acutograve acutorsion acyanopsia acyclic acyesis acyetic acyl acylamido
acylamino acylate acylation acylogen acyloin acyloxy acyrology acystia ada adactyl
adactylia adactylism adactylous adad adage adagial adagietto adagio adai adaize adam
adamant adamantean adamantine adamantoid adamantoma adamas adamastor adamellite adamhood
adamic adamical adamically adamine adamite adamitic adamitical adamitism adamsia
adamsite adance adangle adansonia adapa adapid adapis adapt adaptable adaptation
adaptative adapter adaption adaptional adaptitude adaptive adaptively adaptor adaptorial
adar adarme adat adati adatom adaunt adaw adawe adawlut adawn adaxial aday adays adazzle
adcraft add adda addability addable addax addebted added addedly addend addenda addendum
adder adderbolt adderfish adderspit adderwort addibility addible addicent addict
addicted addiction addie addiment addisonian additament addition additional additive
additively additivity additory addle addlebrain addlehead addlement addleness addlepate
addlepated addleplot addlings addlins addorsed address addressee addresser addressful
addressor addrest addu adduce adducent adducer adducible adduct adduction adductive
adductor addy ade adead adeem adeep adela adelaide adelarthra adelbert adelea adeleidae
adelges adelia adelina adeline adeling adelite adeliza adelopod adelops adelphi
adelphian adelphoi ademonist adempted ademption adenalgia adenalgy adenase adendric
adendritic adenectomy adenia adeniform adenine adenitis adenoblast adenocele adenocyst
adenodynia adenoid adenoidal adenoidism adenology adenoma adenomyoma adenoncus
adenoneure adenopathy adenophora adenophore adenophyma adenose adenosine adenosis
adenostoma adenotome adenotomic adenotomy adenyl adenylic adeodatus adeona adephaga
adephagan adephagia adephagous adept adeptness adeptship adequacy adequate adequately
adequation adequative adermia adermin adet adevism adfected adfix adfluxion adhafera
adhaka adhamant adhara adharma adhere adherence adherency adherent adherently adherer
adhesion adhesional adhesive adhesively adhibit adhibition adiabatic adiabolist
adiactinic adiantum adiaphon adiaphonon adiaphoral adiaphoron adiate adiathetic adiation
adib adicea adicity adiel adieu adieux adigei adighe adigranth adin adinida adinidan
adinole adion adipate adipescent adipic adipinic adipocele adipocere adipocyte
adipogenic adipoid adipolysis adipolytic adipoma adipometer adipopexia adipopexis
adipose adiposis adiposity adiposuria adipous adipsia adipsic adipsous adipsy adipyl
adirondack adit adital aditus adjacency adjacent adjacently adjag adject adjection
adjectival adjective adjiger adjoin adjoined adjoinedly adjoining adjoint adjourn
adjournal adjudge adjudger adjudgment adjudicate adjunct adjunction adjunctive adjunctly
adjuration adjuratory adjure adjurer adjust adjustable adjustably adjustage adjuster
adjustive adjustment adjutage adjutancy adjutant adjutory adjutrice adjuvant adlai adlay
adless adlet adlumia adlumidine adlumine adman admeasure admeasurer admedial admedian
admi admin adminicle adminicula administer admirable admirably admiral admiralty
admiration quasar nebula xylophone fjord
""".split())

if len(VOCAB) != 2048 or len(set(VOCAB)) != 2048:
    raise RuntimeError(f"locked vocab must be 2048 unique words, got {len(VOCAB)}")


def zipf_weights(n, exponent=ZIPF_S):
    return [1.0 / ((i + 1) ** exponent) for i in range(n)]


def generate_rows(rows=ROWS, seed=SEED):
    rng = random.Random(seed)
    weights = zipf_weights(len(VOCAB))
    out = []
    for doc_id in range(1, rows + 1):
        n_title = rng.randint(*TITLE_TOKENS)
        n_body = rng.randint(*BODY_TOKENS)
        title = " ".join(rng.choices(VOCAB, weights=weights, k=n_title))
        body = " ".join(rng.choices(VOCAB, weights=weights, k=n_body))
        out.append((str(doc_id), title, body))
    return out


def corpus_csv_bytes(rows=ROWS, seed=SEED):
    buf = io.StringIO()
    writer = csv.writer(buf, quoting=csv.QUOTE_ALL, lineterminator="\n")
    writer.writerow(["id", "title", "body"])
    writer.writerows(generate_rows(rows, seed))
    return buf.getvalue().encode("utf-8")


def corpus_sha256(rows=ROWS, seed=SEED, data=None):
    payload = corpus_csv_bytes(rows, seed) if data is None else data
    return hashlib.sha256(payload).hexdigest(), payload


def load_queries(path):
    queries = []
    for raw in Path(path).read_text(encoding="utf-8").splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        if "title:(" in line or re.search(r"[A-Za-z_]+:\(", line):
            raise ValueError(f"field syntax is not allowed in GATE queries: {line}")
        queries.append(line)
    if len(queries) != 12:
        raise ValueError(f"expected 12 unscoped queries, got {len(queries)}")
    return queries


def nearest_rank(samples, p):
    """Nearest-rank quantile: k = ceil(p * n), 1-based, on ascending samples."""
    if not samples:
        raise ValueError("no samples")
    ordered = sorted(samples)
    k = math.ceil(p * len(ordered))
    return ordered[k - 1]


def p50_p99(samples):
    return nearest_rank(samples, 0.50), nearest_rank(samples, 0.99)


def stnf_sections(blob):
    """STNF suffix (norms_len, df_len). Both 0 when the blob has no trailer.

    v1 (4.6 / fielded-terms): magic STNF, version 1, norms_len u32le, df_len
    u32le, then norms and df. Prefix is 13 bytes. claimed = 13 + norms + df.

    v2 (STN4): magic STNF, version 2, norms_len u32le, then norms. No df_len
    field. Prefix is 9 bytes. stnf_df_len is 0. claimed = 9 + norms_len.

    The matching prefix must sit at blob EOF. A single-column blob has no
    trailer and returns (0, 0).
    """
    found = None
    start = 0
    while True:
        at = blob.find(STNF_MAGIC, start)
        if at < 0:
            break
        if at + 5 <= len(blob):
            version = blob[at + 4]
            if version == STNF_VERSION_V1 and at + STNF_PREFIX_LEN_V1 <= len(blob):
                norms_len, df_len = struct.unpack_from("<II", blob, at + 5)
                claimed = STNF_PREFIX_LEN_V1 + norms_len + df_len
                if claimed >= STNF_PREFIX_LEN_V1 and at + claimed == len(blob):
                    found = (norms_len, df_len)
            elif version == STNF_VERSION_V2 and at + STNF_PREFIX_LEN_V2 <= len(blob):
                (norms_len,) = struct.unpack_from("<I", blob, at + 5)
                claimed = STNF_PREFIX_LEN_V2 + norms_len
                if claimed >= STNF_PREFIX_LEN_V2 and at + claimed == len(blob):
                    found = (norms_len, 0)
        start = at + 1
    return found if found is not None else (0, 0)


def parse_breakdown(text):
    """Sections.dictionary and blob-byte total from `breakdown` 'as stored'."""
    block = text
    if "as stored:" in text:
        block = text.split("as stored:", 1)[1]
        if "\nre-encoded" in block:
            block = block.split("\nre-encoded", 1)[0]
    match = BREAKDOWN_DICTIONARY.search(block)
    if not match:
        raise ValueError("breakdown output has no dictionary section")
    total = None
    header = text.split("as stored:", 1)[-1].splitlines()[0]
    bytes_match = re.search(r"(\d+) bytes", header)
    if bytes_match:
        total = int(bytes_match.group(1))
    return int(match.group(1)), total


def connect():
    try:
        import psycopg
    except ImportError as error:
        raise SystemExit("psycopg is required for run (benchmarks/requirements.txt)") from error
    return psycopg.connect("", autocommit=True, prepare_threshold=None)


def dump_index_blobs(out_dir, index="idx", dump_segments=DUMP_SEGMENTS, dbname=None):
    cmd = [sys.executable, str(dump_segments), "--index", index, "--out", str(out_dir)]
    if dbname:
        cmd += ["--dbname", dbname]
    subprocess.run(cmd, check=True)


def run_breakdown(blobs, repo=ROOT):
    cmd = [
        "cargo",
        "run",
        "-p",
        "segment",
        "--release",
        "--example",
        "breakdown",
        "--",
        *[str(path) for path in blobs],
    ]
    result = subprocess.run(
        cmd, cwd=repo, check=False, text=True, capture_output=True
    )
    if result.returncode != 0:
        raise RuntimeError(
            "breakdown failed:\n"
            f"stdout:\n{result.stdout}\n"
            f"stderr:\n{result.stderr}"
        )
    return parse_breakdown(result.stdout), result.stdout


def measure_dictionary(blobs, repo=ROOT):
    per_segment = []
    stnf_df = 0
    stnf_norms = 0
    trailers = 0
    segment_bytes = 0
    for path in blobs:
        blob = Path(path).read_bytes()
        segment_bytes += len(blob)
        norms_len, df_len = stnf_sections(blob)
        if norms_len or df_len:
            trailers += 1
        stnf_df += df_len
        stnf_norms += norms_len
        per_segment.append(
            {
                "path": Path(path).name,
                "bytes": len(blob),
                "stnf_norms_len": norms_len,
                "stnf_df_len": df_len,
            }
        )
    if not blobs:
        return {
            "dict_bytes": 0,
            "dict_breakdown": {
                "sections_dictionary": 0,
                "stnf_df_len": 0,
                "stnf_norms_len": 0,
                "trailer_count": 0,
                "per_segment": [],
            },
            "segment_bytes": 0,
            "stnf": {"trailer_count": 0, "df_len_sum": 0, "norms_len_sum": 0},
        }
    (dictionary, breakdown_bytes), _ = run_breakdown(blobs, repo=repo)
    dict_bytes = dictionary + stnf_df
    return {
        "dict_bytes": dict_bytes,
        "dict_breakdown": {
            "sections_dictionary": dictionary,
            "stnf_df_len": stnf_df,
            "stnf_norms_len": stnf_norms,
            "trailer_count": trailers,
            "breakdown_blob_bytes": breakdown_bytes,
            "per_segment": per_segment,
        },
        "segment_bytes": segment_bytes,
        "stnf": {
            "trailer_count": trailers,
            "df_len_sum": stnf_df,
            "norms_len_sum": stnf_norms,
        },
    }


def time_search(conn, query):
    start = time.perf_counter()
    rows = conn.execute(TIMED_SQL, (query,)).fetchall()
    elapsed = time.perf_counter() - start
    return elapsed, len(rows)


def measure_queries(conn, queries):
    cold = {}
    timed = {}
    for query in queries:
        elapsed, n = time_search(conn, query)
        cold[query] = {"s": elapsed, "rows": n}
    for query in queries:
        time_search(conn, query)
        samples = []
        last_rows = None
        for _ in range(TIMED_RUNS):
            elapsed, n = time_search(conn, query)
            samples.append(elapsed)
            last_rows = n
        p50, p99 = p50_p99(samples)
        timed[query] = {
            "samples_s": samples,
            "p50_s": p50,
            "p99_s": p99,
            "rows": last_rows,
            "cold_s": cold[query]["s"],
        }
    return timed


def load_table(conn, csv_bytes):
    conn.execute("DROP TABLE IF EXISTS documents CASCADE")
    conn.execute(
        """
        CREATE TABLE documents (
            id text PRIMARY KEY,
            title text NOT NULL,
            body text NOT NULL,
            concat text GENERATED ALWAYS AS (title || ' ' || body) STORED
        )
        """
    )
    with conn.cursor() as cur:
        with cur.copy(
            "COPY documents (id, title, body) FROM STDIN WITH (FORMAT csv, HEADER true)"
        ) as copy:
            copy.write(csv_bytes)


def git_sha(repo=ROOT):
    try:
        return subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=repo, text=True
        ).strip()
    except (subprocess.CalledProcessError, FileNotFoundError):
        return None


def mean(values):
    return sum(values) / len(values) if values else None


def decide(fielded, single):
    """Apply the locked GATE table. Does not invent a p50 waiver."""
    missing = []
    if fielded is None:
        missing.append("stn3_fielded")
    if single is None:
        missing.append("stn3_single")
    if missing:
        return {
            "decision": "escalate",
            "mandatory_gate": {
                "pass": False,
                "dict_ratio": None,
                "build_ratio": None,
                "threshold": MANDATORY_THRESHOLD,
            },
            "advisory_p50_gate": {
                "pass": False,
                "ratio": None,
                "threshold": ADVISORY_P50_THRESHOLD,
                "waiver": None,
            },
            "blocked": missing,
        }
    dict_den = single.get("dict_bytes") or 0
    build_den = single.get("build_s") or 0
    if dict_den <= 0 or build_den <= 0:
        return {
            "decision": "escalate",
            "mandatory_gate": {
                "pass": False,
                "dict_ratio": None,
                "build_ratio": None,
                "threshold": MANDATORY_THRESHOLD,
            },
            "advisory_p50_gate": {
                "pass": False,
                "ratio": None,
                "threshold": ADVISORY_P50_THRESHOLD,
                "waiver": None,
            },
            "blocked": ["cannot measure: zero single-column denominator"],
        }
    dict_ratio = fielded["dict_bytes"] / dict_den
    build_ratio = fielded["build_s"] / build_den
    mandatory = dict_ratio <= MANDATORY_THRESHOLD and build_ratio <= MANDATORY_THRESHOLD
    fielded_p50 = [q["p50_s"] for q in fielded.get("queries", {}).values()]
    single_p50 = [q["p50_s"] for q in single.get("queries", {}).values()]
    p50_ratio = None
    if fielded_p50 and single_p50 and mean(single_p50):
        p50_ratio = mean(fielded_p50) / mean(single_p50)
    advisory = p50_ratio is not None and p50_ratio <= ADVISORY_P50_THRESHOLD
    if not mandatory:
        decision = "STN4"
    elif p50_ratio is None:
        decision = "escalate"
    elif not advisory:
        decision = "escalate"
    else:
        decision = "fielded-terms"
    return {
        "decision": decision,
        "mandatory_gate": {
            "pass": mandatory,
            "dict_ratio": dict_ratio,
            "build_ratio": build_ratio,
            "threshold": MANDATORY_THRESHOLD,
        },
        "advisory_p50_gate": {
            "pass": advisory,
            "ratio": p50_ratio,
            "threshold": ADVISORY_P50_THRESHOLD,
            "waiver": None,
        },
    }


def ratios_block(systems):
    fielded = systems.get("stn3_fielded") or {}
    single = systems.get("stn3_single") or {}
    v040 = systems.get("v040") or {}

    def ratio(num, den, key):
        n, d = num.get(key), den.get(key)
        if n is None or not d:
            return None
        return n / d

    def p50_ratio(num, den):
        n = [q["p50_s"] for q in (num.get("queries") or {}).values()]
        d = [q["p50_s"] for q in (den.get("queries") or {}).values()]
        if not n or not d or not mean(d):
            return None
        return mean(n) / mean(d)

    return {
        "dict_fielded_over_single": ratio(fielded, single, "dict_bytes"),
        "build_fielded_over_single": ratio(fielded, single, "build_s"),
        "p50_fielded_over_single": p50_ratio(fielded, single),
        "dict_fielded_over_v040": ratio(fielded, v040, "dict_bytes"),
        "build_fielded_over_v040": ratio(fielded, v040, "build_s"),
        "p50_fielded_over_v040": p50_ratio(fielded, v040),
        "p50_single_over_v040": p50_ratio(single, v040),
        "note": "0.4.0 comparison is reported, not gated",
    }


def cmd_generate(args):
    sha, payload = corpus_sha256(rows=args.rows, seed=args.seed)
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_bytes(payload)
        print(f"path: {args.out}")
    print(f"sha256: {sha}")
    print(f"rows: {args.rows}")
    print(f"seed: {args.seed}")
    return 0


def cmd_run(args):
    sha, payload = corpus_sha256()
    if args.corpus:
        got = Path(args.corpus).read_bytes()
        got_sha = hashlib.sha256(got).hexdigest()
        if got_sha != sha:
            raise SystemExit(f"corpus sha256 {got_sha} != locked {sha}")
        payload = got
    queries = load_queries(args.queries)
    conn = connect()
    try:
        server_version = conn.execute("SHOW server_version").fetchone()[0]
        conn.execute("CREATE EXTENSION IF NOT EXISTS stannum")
        load_table(conn, payload)
        create_sql = INDEX_SQL[args.system]
        started = time.perf_counter()
        conn.execute(create_sql)
        build_s = time.perf_counter() - started
        conn.execute("CHECKPOINT")
        with tempfile.TemporaryDirectory(prefix="stn3-46-blobs-") as raw:
            out_dir = Path(raw)
            dump_index_blobs(
                out_dir,
                index="idx",
                dump_segments=args.dump_segments,
                dbname=os.environ.get("PGDATABASE"),
            )
            blobs = sorted(out_dir.glob("*.segment"))
            sizes = measure_dictionary(blobs, repo=args.repo)
        timed = measure_queries(conn, queries)
    finally:
        conn.close()
    fragment = {
        "system": args.system,
        "server_version": server_version,
        "corpus": {"rows": ROWS, "seed": SEED, "sha256": sha, "columns": ["id", "title", "body"]},
        "create_index_sql": create_sql,
        "timed_sql": TIMED_SQL,
        "build_s": build_s,
        **sizes,
        "queries": timed,
        "protocol": {
            "warmup": WARMUP_RUNS,
            "timed_runs": TIMED_RUNS,
            "concurrency": 1,
            "gucs": "defaults",
            "nearest_rank": "p50=ceil(0.50*n)th, p99=ceil(0.99*n)th; n=20 → 10th and 20th",
            "cold": (
                "first SELECT of each query after CREATE INDEX and CHECKPOINT; "
                "not mixed into the 20 timed samples"
            ),
        },
    }
    args.fragment.parent.mkdir(parents=True, exist_ok=True)
    args.fragment.write_text(json.dumps(fragment, indent=2) + "\n")
    print(f"wrote {args.fragment}")
    print(f"build_s={build_s:.4f} dict_bytes={sizes['dict_bytes']} segments={len(sizes['dict_breakdown']['per_segment'])}")
    return 0


def cmd_merge(args):
    systems = {}
    for name, path in (
        ("stn3_fielded", args.fielded),
        ("stn3_single", args.single),
        ("v040", args.v040),
    ):
        if path is None:
            systems[name] = None
            continue
        systems[name] = json.loads(Path(path).read_text(encoding="utf-8"))
    gate = decide(systems.get("stn3_fielded"), systems.get("stn3_single"))
    corpus = None
    for fragment in systems.values():
        if fragment and fragment.get("corpus"):
            corpus = fragment["corpus"]
            break
    if corpus is None:
        sha, _ = corpus_sha256()
        corpus = {"rows": ROWS, "seed": SEED, "sha256": sha, "columns": ["id", "title", "body"]}
    pg = args.pg_version
    for fragment in systems.values():
        if fragment and fragment.get("server_version"):
            pg = pg or fragment["server_version"]
            break
    result = {
        "decision": gate["decision"],
        "mandatory_gate": gate["mandatory_gate"],
        "advisory_p50_gate": gate["advisory_p50_gate"],
        "corpus": corpus,
        "chinese_corpus": "deferred",
        "protocol": {
            "pg": pg or "17.11",
            "concurrency": 1,
            "gucs": "defaults",
            "warmup": WARMUP_RUNS,
            "timed_runs": TIMED_RUNS,
            "nearest_rank": "p50=ceil(0.50*n)th, p99=ceil(0.99*n)th of sorted samples; n=20 → 10th and 20th",
            "cold": (
                "labeled first SELECT of each query after CREATE INDEX and CHECKPOINT; "
                "not mixed into the 20"
            ),
            "server_topology": args.topology,
            "timed_sql": TIMED_SQL,
            "search_limit": SEARCH_LIMIT,
        },
        "systems": {
            "stn3_fielded": systems.get("stn3_fielded"),
            "stn3_single": systems.get("stn3_single"),
            "v040": systems.get("v040"),
        },
        "ratios": ratios_block(systems),
        "git": {
            "stn3": args.stn3_sha or git_sha(),
            "v040": args.v040_sha,
        },
    }
    if gate.get("blocked"):
        result["blocked"] = gate["blocked"]
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(result, indent=2) + "\n")
    print(f"wrote {args.out}")
    print(f"decision={result['decision']}")
    return 0


def build_parser():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = parser.add_subparsers(dest="command", required=True)

    gen = sub.add_parser("generate", help="write or hash the locked synthetic CSV")
    gen.add_argument("--out", type=Path, help="CSV path; omit to hash only (dry-run)")
    gen.add_argument("--rows", type=int, default=ROWS)
    gen.add_argument("--seed", type=int, default=SEED)
    gen.set_defaults(func=cmd_generate)

    run = sub.add_parser("run", help="load, CREATE INDEX, dump, and time one system")
    run.add_argument("--system", required=True, choices=SYSTEMS)
    run.add_argument("--fragment", type=Path, required=True, help="write this system JSON fragment")
    run.add_argument("--corpus", type=Path, help="locked CSV; omit to generate in memory")
    run.add_argument("--queries", type=Path, default=DEFAULT_QUERIES)
    run.add_argument("--dump-segments", type=Path, default=DUMP_SEGMENTS)
    run.add_argument("--repo", type=Path, default=ROOT, help="cargo working directory")
    run.set_defaults(func=cmd_run)

    merge = sub.add_parser("merge", help="merge three fragments and apply the GATE table")
    merge.add_argument("--fielded", type=Path, help="stn3_fielded fragment")
    merge.add_argument("--single", type=Path, help="stn3_single fragment")
    merge.add_argument("--v040", type=Path, help="v040 fragment")
    merge.add_argument("--out", type=Path, default=DEFAULT_JSON)
    merge.add_argument("--stn3-sha", help="stn3 HEAD SHA (default: git rev-parse HEAD)")
    merge.add_argument("--v040-sha", default=V040_PIN)
    merge.add_argument("--pg-version", help="PostgreSQL version string (default: from fragments or 17.11)")
    merge.add_argument(
        "--topology",
        default="unspecified; Item 2 records Homebrew sequential vs isolated prefixes",
        help="server topology notes for the JSON protocol block",
    )
    merge.set_defaults(func=cmd_merge)
    return parser


def main(argv=None):
    parser = build_parser()
    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
