use std::sync::LazyLock;

use regex::Regex;

static VENDOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^(?:(?:[Dd]ependencies/)|(?:debian/)|(?:deps/)|(?:rebar$)))|(?:(?:^|/)(?:(?:BuddyBuildSDK\.framework/)|(?:Carthage/)|(?:Chart\.js$)|(?:Control\.FullScreen\.css)|(?:Control\.FullScreen\.js)|(?:Crashlytics\.framework/)|(?:Fabric\.framework/)|(?:Godeps/_workspace/)|(?:Jenkinsfile$)|(?:Leaflet\.Coordinates-\d+\.\d+\.\d+\.src\.js$)|(?:MathJax/)|(?:MochiKit\.js$)|(?:RealmSwift\.framework)|(?:Realm\.framework)|(?:Sparkle/)|(?:Vagrantfile$)|(?:[Bb]ourbon/.*\.(css|less|scss|styl)$)|(?:[Cc]ode[Mm]irror/(\d+\.\d+/)?(lib|mode|theme|addon|keymap|demo))|(?:[Ee]xtern(als?)?/)|(?:[Mm]icrosoft([Mm]vc)?([Aa]jax|[Vv]alidation)(\.debug)?\.js$)|(?:[Pp]ackages/.+\.\d+/)|(?:[Ss]pecs?/fixtures/)|(?:[Tt]ests?/fixtures/)|(?:[Vv]+endor/)|(?:\.[Dd][Ss]_[Ss]tore$)|(?:\.gitattributes$)|(?:\.github/)|(?:\.gitignore$)|(?:\.gitmodules$)|(?:\.gitpod\.Dockerfile$)|(?:\.google_apis/)|(?:\.indent\.pro)|(?:\.mvn/wrapper/)|(?:\.obsidian/)|(?:\.osx$)|(?:\.sublime-project)|(?:\.sublime-workspace)|(?:\.teamcity/)|(?:\.vscode/)|(?:\.yarn/plugins/)|(?:\.yarn/releases/)|(?:\.yarn/sdks/)|(?:\.yarn/unplugged/)|(?:\.yarn/versions/)|(?:_esy$)|(?:ace-builds/)|(?:aclocal\.m4)|(?:activator$)|(?:activator\.bat$)|(?:admin_media/)|(?:angular([^.]*)\.js$)|(?:animate\.(css|less|scss|styl)$)|(?:bootbox\.js)|(?:bootstrap([^/.]*)(\..*)?\.(js|css|less|scss|styl)$)|(?:bootstrap-datepicker/)|(?:bower_components/)|(?:bulma\.(css|sass|scss)$)|(?:cache/)|(?:ckeditor\.js$)|(?:config\.guess$)|(?:config\.sub$)|(?:configure$)|(?:controls\.js$)|(?:cordova([^.]*)\.js$)|(?:cordova\-\d\.\d(\.\d)?\.js$)|(?:cpplint\.py)|(?:custom\.bootstrap([^\s]*)(js|css|less|scss|styl)$)|(?:dist/)|(?:docs?/_?(build|themes?|templates?|static)/)|(?:dojo\.js$)|(?:dotnet-install\.(ps1|sh)$)|(?:dragdrop\.js$)|(?:effects\.js$)|(?:env/)|(?:erlang\.mk)|(?:extjs/.*?\.html$)|(?:extjs/.*?\.js$)|(?:extjs/.*?\.properties$)|(?:extjs/.*?\.txt$)|(?:extjs/.*?\.xml$)|(?:extjs/\.sencha/)|(?:extjs/builds/)|(?:extjs/cmd/)|(?:extjs/docs/)|(?:extjs/examples/)|(?:extjs/locale/)|(?:extjs/packages/)|(?:extjs/plugins/)|(?:extjs/resources/)|(?:extjs/src/)|(?:extjs/welcome/)|(?:fabfile\.py$)|(?:flow-typed/.*\.js$)|(?:font-?awesome/.*\.(css|less|scss|styl)$)|(?:font-?awesome\.(css|less|scss|styl)$)|(?:fontello(.*?)\.css$)|(?:foundation(\..*)?\.js$)|(?:foundation\.(css|less|scss|styl)$)|(?:fuelux\.js)|(?:gradle/wrapper/)|(?:gradlew$)|(?:gradlew\.bat$)|(?:html5shiv\.js$)|(?:inst/extdata/)|(?:jquery([^.]*)\.js$)|(?:jquery([^.]*)\.unobtrusive\-ajax\.js$)|(?:jquery([^.]*)\.validate(\.unobtrusive)?\.js$)|(?:jquery\-\d\.\d+(\.\d+)?\.js$)|(?:jquery\-ui(\-\d\.\d+(\.\d+)?)?(\.\w+)?\.(js|css)$)|(?:jquery\.(ui|effects)\.([^.]*)\.(js|css)$)|(?:jquery\.dataTables\.js)|(?:jquery\.fancybox\.(js|css))|(?:jquery\.fileupload(-\w+)?\.js$)|(?:jquery\.fn\.gantt\.js)|(?:knockout-(\d+\.){3}(debug\.)?js$)|(?:leaflet\.draw-src\.js)|(?:leaflet\.draw\.css)|(?:leaflet\.spin\.js)|(?:libtool\.m4)|(?:ltoptions\.m4)|(?:ltsugar\.m4)|(?:ltversion\.m4)|(?:lt~obsolete\.m4)|(?:materialize\.(css|less|scss|styl|js)$)|(?:modernizr\-\d\.\d+(\.\d+)?\.js$)|(?:modernizr\.custom\.\d+\.js$)|(?:mootools([^.]*)\d+\.\d+.\d+([^.]*)\.js$)|(?:mvnw$)|(?:mvnw\.cmd$)|(?:node_modules/)|(?:normalize\.(css|less|scss|styl)$)|(?:octicons\.css)|(?:pdf\.worker\.js)|(?:proguard-rules\.pro$)|(?:proguard\.pro$)|(?:prototype(.*)\.js$)|(?:puphpet/)|(?:react(-[^.]*)?\.js$)|(?:run\.n$)|(?:select2/.*\.(css|scss|js)$)|(?:shBrush([^.]*)\.js$)|(?:shCore\.js$)|(?:shLegacy\.js$)|(?:skeleton\.(css|less|scss|styl)$)|(?:slick\.\w+.js$)|(?:sprockets-octicons\.scss)|(?:testdata/)|(?:tiny_mce([^.]*)\.js$)|(?:tiny_mce/(langs|plugins|themes|utils))|(?:vendors?/)|(?:waf$)|(?:wicket-leaflet\.js)|(?:xvba_modules/)|(?:yahoo-([^.]*)\.js$)|(?:yui([^.]*)\.js$)))|(?:(.*?)\.d\.ts$)|(?:(3rd|[Tt]hird)[-_]?[Pp]arty/)|(?:([^\s]*)import\.(css|less|scss|styl)$)|(?:(\.|-)min\.(js|css)$)|(?:(^|/)d3(\.v\d+)?([^.]*)\.js$)|(?:-vsdoc\.js$)|(?:\.imageset/)|(?:\.intellisense\.js$)|(?:\.xctemplate/)").expect("enry vendor regex compiles")
});

static DOCUMENTATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^[Dd]ocs?/)|(?:(^|/)[Dd]ocumentation/)|(?:(^|/)[Gg]roovydoc/)|(?:(^|/)[Jj]avadoc/)|(?:^[Mm]an/)|(?:^[Ee]xamples/)|(?:^[Dd]emos?/)|(?:(^|/)inst/doc/)|(?:(^|/)CITATION(\.cff|(S)?(\.(bib|md))?)$)|(?:(^|/)CHANGE(S|LOG)?(\.|$))|(?:(^|/)CONTRIBUTING(\.|$))|(?:(^|/)COPYING(\.|$))|(?:(^|/)INSTALL(\.|$))|(?:(^|/)LICEN[CS]E(\.|$))|(?:(^|/)[Ll]icen[cs]e(\.|$))|(?:(^|/)README(\.|$))|(?:(^|/)[Rr]eadme(\.|$))|(?:^[Ss]amples?/)").expect("enry documentation regex compiles")
});

static GENERATED_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:(?:^|/)\.idea/)|(?:(^Pods|/Pods)/)|(?:(^|/)Carthage/Build/)|(?:(?i)\.designer\.(cs|vb)$)|(?:(?i)\.feature\.cs$)|(?:vendor/([-0-9A-Za-z]+\.)+(com|edu|gov|in|me|net|org|fm|io))|(?:(^|/)(\w+\.)?esy.lock$)|(?:(^|/)\.pnp\..*$)|(?:.\.zep\.(?:c|h|php)$)|(?:(^|/)flake\.lock$)|(?:(^|/)MODULE\.bazel\.lock$)|(?:(?:^|/)\.terraform\.lock\.hcl$)|(?:(?i)_tlb\.pas$)|(?:(?:^|/)htmlcov/)|(?:(?:^|.*/)\.sqlx/query-.+\.json$)").expect("enry generated-name regex compiles")
});

const GENERATED_SUFFIXES: &[&str] = &[
    "Gopkg.lock",
    "glide.lock",
    "poetry.lock",
    "pdm.lock",
    "uv.lock",
    "deno.lock",
    "npm-shrinkwrap.json",
    "package-lock.json",
    "pnpm-lock.yaml",
    "composer.lock",
    "Cargo.lock",
    "Cargo.toml.orig",
    "Pipfile.lock",
    "bun.lock",
];

const GENERATED_CONTAINS: &[&str] = &["node_modules/", "Godeps/", "__generated__/"];

const GENERATED_EXTENSIONS: &[&str] = &[".nib", ".xcworkspacedata", ".xcuserstate"];

fn is_dotfile(path: &str) -> bool {
    path.rsplit('/')
        .next()
        .is_some_and(|base| base.starts_with('.') && base != ".")
}

fn is_generated_name(path: &str) -> bool {
    GENERATED_SUFFIXES
        .iter()
        .any(|suffix| path.ends_with(suffix))
        || GENERATED_CONTAINS
            .iter()
            .any(|needle| path.contains(needle))
        || GENERATED_EXTENSIONS
            .iter()
            .any(|extension| path.ends_with(extension))
        || GENERATED_NAME.is_match(path)
}

pub(crate) fn is_skipped_path(path: &str) -> bool {
    is_dotfile(path)
        || VENDOR.is_match(path)
        || DOCUMENTATION.is_match(path)
        || is_generated_name(path)
}

pub(crate) fn is_vendor_dir(path: &str) -> bool {
    VENDOR.is_match(&format!("{path}/"))
}

fn language_group(name: &str) -> Option<&'static str> {
    match name {
        "Alpine Abuild" => Some("Shell"),
        "Apollo Guidance Computer" => Some("Assembly"),
        "BibTeX" => Some("TeX"),
        "Bison" => Some("Yacc"),
        "Bluespec BH" => Some("Bluespec"),
        "C2hs Haskell" => Some("Haskell"),
        "Cairo" => Some("Cairo"),
        "Cairo Zero" => Some("Cairo"),
        "CameLIGO" => Some("LigoLANG"),
        "ColdFusion CFC" => Some("ColdFusion"),
        "Cylc" => Some("INI"),
        "ECLiPSe" => Some("Prolog"),
        "Easybuild" => Some("Python"),
        "Ecere Projects" => Some("JavaScript"),
        "Ecmarkup" => Some("HTML"),
        "EditorConfig" => Some("INI"),
        "Elvish Transcript" => Some("Elvish"),
        "Filterscript" => Some("RenderScript"),
        "Fortran" => Some("Fortran"),
        "Fortran Free Form" => Some("Fortran"),
        "Gentoo Ebuild" => Some("Shell"),
        "Gentoo Eclass" => Some("Shell"),
        "Git Config" => Some("INI"),
        "Glimmer JS" => Some("JavaScript"),
        "Glimmer TS" => Some("TypeScript"),
        "Gradle Kotlin DSL" => Some("Gradle"),
        "Groovy Server Pages" => Some("Groovy"),
        "HTML+ECR" => Some("HTML"),
        "HTML+EEX" => Some("HTML"),
        "HTML+ERB" => Some("HTML"),
        "HTML+PHP" => Some("HTML"),
        "HTML+Razor" => Some("HTML"),
        "Isabelle ROOT" => Some("Isabelle"),
        "JFlex" => Some("Lex"),
        "JSON with Comments" => Some("JSON"),
        "Java Server Pages" => Some("Java"),
        "Java Template Engine" => Some("Java"),
        "JavaScript+ERB" => Some("JavaScript"),
        "Jison" => Some("Yacc"),
        "Jison Lex" => Some("Lex"),
        "Julia REPL" => Some("Julia"),
        "Lean 4" => Some("Lean"),
        "LigoLANG" => Some("LigoLANG"),
        "Literate Agda" => Some("Agda"),
        "Literate CoffeeScript" => Some("CoffeeScript"),
        "Literate Haskell" => Some("Haskell"),
        "M4Sugar" => Some("M4"),
        "MUF" => Some("Forth"),
        "Maven POM" => Some("XML"),
        "Motorola 68K Assembly" => Some("Assembly"),
        "NPM Config" => Some("INI"),
        "NumPy" => Some("Python"),
        "OASv2-json" => Some("OpenAPI Specification v2"),
        "OASv2-yaml" => Some("OpenAPI Specification v2"),
        "OASv3-json" => Some("OpenAPI Specification v3"),
        "OASv3-yaml" => Some("OpenAPI Specification v3"),
        "OpenCL" => Some("C"),
        "OpenRC runscript" => Some("Shell"),
        "Parrot Assembly" => Some("Parrot"),
        "Parrot Internal Representation" => Some("Parrot"),
        "Pic" => Some("Roff"),
        "PostCSS" => Some("CSS"),
        "Python console" => Some("Python"),
        "Python traceback" => Some("Python"),
        "RBS" => Some("Ruby"),
        "Readline Config" => Some("INI"),
        "ReasonLIGO" => Some("LigoLANG"),
        "Roff Manpage" => Some("Roff"),
        "SSH Config" => Some("INI"),
        "STON" => Some("Smalltalk"),
        "Simple File Verification" => Some("Checksums"),
        "Snakemake" => Some("Python"),
        "TSX" => Some("TypeScript"),
        "Tcsh" => Some("Shell"),
        "Terraform Template" => Some("HCL"),
        "Unified Parallel C" => Some("C"),
        "Unix Assembly" => Some("Assembly"),
        "Wget Config" => Some("INI"),
        "X BitMap" => Some("C"),
        "X PixMap" => Some("C"),
        "XML Property List" => Some("XML"),
        "cURL Config" => Some("INI"),
        "fish" => Some("Shell"),
        "nanorc" => Some("INI"),
        _ => None,
    }
}

pub(crate) fn group(name: &'static str) -> &'static str {
    language_group(name).unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enry_matchers_compile_and_match_known_paths() {
        assert!(is_skipped_path("node_modules/left-pad/index.js"));
        assert!(is_skipped_path("src/jquery-3.6.0.js"));
        assert!(is_skipped_path("docs/guide.md"));
        assert!(is_skipped_path("README.md"));
        assert!(is_skipped_path("LICENSE"));
        assert!(is_skipped_path("Cargo.lock"));
        assert!(is_skipped_path("package-lock.json"));
        assert!(is_skipped_path("app/main.designer.cs"));
        assert!(is_skipped_path(".github/workflows/ci.yml"));
        assert!(is_skipped_path("third_party/zlib/zlib.c"));
        assert!(is_skipped_path("web/app.min.js"));
        assert!(!is_skipped_path("src/main.rs"));
        assert!(!is_skipped_path("internal/server.go"));
    }

    #[test]
    fn vendor_dir_pruning_matches_directory_paths() {
        assert!(is_vendor_dir("node_modules"));
        assert!(is_vendor_dir("a/b/dist"));
        assert!(!is_vendor_dir("src"));
    }

    #[test]
    fn grouping_folds_known_languages() {
        assert_eq!(group("TSX"), "TypeScript");
        assert_eq!(group("HTML+ERB"), "HTML");
        assert_eq!(group("Rust"), "Rust");
    }
}
