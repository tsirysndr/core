package pages

import (
	"context"
	"crypto/sha256"
	"embed"
	"encoding/hex"
	"fmt"
	"html/template"
	"io"
	"io/fs"
	"log/slog"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/cache"
	"tangled.org/core/appview/commitverify"
	"tangled.org/core/appview/config"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/oauth"
	"tangled.org/core/appview/pages/markup"
	"tangled.org/core/appview/pages/markup/sanitizer"
	"tangled.org/core/appview/pages/repoinfo"
	"tangled.org/core/appview/pagination"
	"tangled.org/core/idresolver"
	"tangled.org/core/types"

	"github.com/bluesky-social/indigo/atproto/identity"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/go-git/go-git/v5/plumbing"
	"github.com/sourcegraph/zoekt"
)

//go:embed templates/* static legal
var Files embed.FS

type baseParamsCtxKey struct{}

type BaseParams struct {
	LoggedInUser    *oauth.MultiAccountUser
	FocusParams     FocusParams
	ThemePreference models.ThemePreference
}

type FocusParams struct {
	Focusing            bool
	FocusLink           string
	FocusNotificationID int64
	CurrentPath         string // r.URL.Path, for off-focus detection in templates
	FocusCount          int    // total unread focus-eligible items remaining
}

func (p *Pages) Resolver() *idresolver.Resolver {
	return p.resolver
}

func BaseParamsIntoContext(ctx context.Context, bp BaseParams) context.Context {
	return context.WithValue(ctx, baseParamsCtxKey{}, bp)
}

func BaseParamsFromContext(ctx context.Context) BaseParams {
	bp, _ := ctx.Value(baseParamsCtxKey{}).(BaseParams)
	return bp
}

type Pages struct {
	mu    sync.RWMutex
	cache *TmplCache[string, *template.Template]

	avatar      config.AvatarConfig
	pdsCfg      config.PdsConfig
	resolver    *idresolver.Resolver
	db          *db.DB
	rdb         *cache.Cache
	dev         bool
	embedFS     fs.FS
	templateDir string // Path to templates on disk for dev mode
	rctx        *markup.RenderContext
	logger      *slog.Logger
}

func NewPages(config *config.Config, res *idresolver.Resolver, database *db.DB, rdb *cache.Cache, logger *slog.Logger) *Pages {
	// initialized with safe defaults, can be overridden per use
	rctx := &markup.RenderContext{
		IsDev:      config.Core.Dev,
		Hostname:   config.Core.AppviewHost,
		CamoUrl:    config.Camo.Host,
		CamoSecret: config.Camo.SharedSecret,
		Directory:  res.Directory(),
	}

	p := &Pages{
		mu:          sync.RWMutex{},
		cache:       NewTmplCache[string, *template.Template](),
		dev:         config.Core.Dev,
		avatar:      config.Avatar,
		pdsCfg:      config.Pds,
		rctx:        rctx,
		resolver:    res,
		db:          database,
		rdb:         rdb,
		templateDir: "appview/pages",
		logger:      logger,
	}

	if p.dev {
		p.embedFS = os.DirFS(p.templateDir)
	} else {
		p.embedFS = Files
	}

	return p
}

// reverse of pathToName
func (p *Pages) nameToPath(s string) string {
	return "templates/" + s + ".html"
}

// FuncMap returns the template function map for use by external template consumers.
func (p *Pages) FuncMap() template.FuncMap {
	return p.funcMap()
}

// FragmentPaths returns all fragment template paths from the embedded FS.
func (p *Pages) FragmentPaths() ([]string, error) {
	return p.fragmentPaths()
}

// EmbedFS returns the embedded filesystem containing templates and static assets.
func (p *Pages) EmbedFS() fs.FS {
	return p.embedFS
}

func (p *Pages) fragmentPaths() ([]string, error) {
	var fragmentPaths []string
	err := fs.WalkDir(p.embedFS, "templates", func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if d.IsDir() {
			return nil
		}
		if !strings.HasSuffix(path, ".html") {
			return nil
		}
		if !strings.Contains(path, "fragments/") {
			return nil
		}
		fragmentPaths = append(fragmentPaths, path)
		return nil
	})
	if err != nil {
		return nil, err
	}

	return fragmentPaths, nil
}

// parse without memoization
func (p *Pages) rawParse(stack ...string) (*template.Template, error) {
	paths, err := p.fragmentPaths()
	if err != nil {
		return nil, err
	}
	for _, s := range stack {
		paths = append(paths, p.nameToPath(s))
	}

	funcs := p.funcMap()
	top := stack[len(stack)-1]
	parsed, err := template.New(top).
		Funcs(funcs).
		ParseFS(p.embedFS, paths...)
	if err != nil {
		return nil, err
	}

	return parsed, nil
}

func (p *Pages) parse(stack ...string) (*template.Template, error) {
	key := strings.Join(stack, "|")

	// never cache in dev mode
	if cached, exists := p.cache.Get(key); !p.dev && exists {
		return cached, nil
	}

	result, err := p.rawParse(stack...)
	if err != nil {
		return nil, err
	}

	p.cache.Set(key, result)
	return result, nil
}

func (p *Pages) parseBase(top string) (*template.Template, error) {
	stack := []string{
		"layouts/base",
		top,
	}
	return p.parse(stack...)
}

func (p *Pages) parseRepoBase(top string) (*template.Template, error) {
	stack := []string{
		"layouts/base",
		"layouts/repobase",
		top,
	}
	return p.parse(stack...)
}

func (p *Pages) parseProfileBase(top string) (*template.Template, error) {
	stack := []string{
		"layouts/base",
		"layouts/profilebase",
		top,
	}
	return p.parse(stack...)
}

func (p *Pages) parseLoginBase(top string) (*template.Template, error) {
	stack := []string{
		"layouts/base",
		"layouts/loginbase",
		top,
	}
	return p.parse(stack...)
}

func (p *Pages) parseOnboardingBase(top string) (*template.Template, error) {
	stack := []string{
		"layouts/base",
		"layouts/onboardingbase",
		top,
	}
	return p.parse(stack...)
}

func (p *Pages) executePlain(name string, w io.Writer, params any) error {
	tpl, err := p.parse(name)
	if err != nil {
		return err
	}

	err = tpl.Execute(w, params)
	if err != nil {
		p.logger.Error("failed to execute template", "template", name, "err", err)
	}
	return err
}

func (p *Pages) executeLogin(name string, w io.Writer, params any) error {
	tpl, err := p.parseLoginBase(name)
	if err != nil {
		return err
	}

	err = tpl.ExecuteTemplate(w, "layouts/base", params)
	if err != nil {
		p.logger.Error("failed to execute login template", "template", name, "err", err)
	}
	return err
}

func (p *Pages) executeOnboarding(name string, w io.Writer, params any) error {
	tpl, err := p.parseOnboardingBase(name)
	if err != nil {
		return err
	}

	return tpl.ExecuteTemplate(w, "layouts/base", params)
}

func (p *Pages) execute(name string, w io.Writer, params any) error {
	tpl, err := p.parseBase(name)
	if err != nil {
		return err
	}

	err = tpl.ExecuteTemplate(w, "layouts/base", params)
	if err != nil {
		p.logger.Error("failed to execute template", "template", name, "err", err)
	}
	return err
}

func (p *Pages) executeRepo(name string, w io.Writer, params any) error {
	tpl, err := p.parseRepoBase(name)
	if err != nil {
		return err
	}

	err = tpl.ExecuteTemplate(w, "layouts/base", params)
	if err != nil {
		p.logger.Error("failed to execute repo template", "template", name, "err", err)
	}
	return err
}

func (p *Pages) executeProfile(name string, w io.Writer, params any) error {
	tpl, err := p.parseProfileBase(name)
	if err != nil {
		return err
	}

	err = tpl.ExecuteTemplate(w, "layouts/base", params)
	if err != nil {
		p.logger.Error("failed to execute profile template", "template", name, "err", err)
	}
	return err
}

type DollyParams struct {
	Classes   string
	FillColor string
	// Favicon embeds a prefers-color-scheme style block so the SVG
	// adapts to dark mode when used as a standalone favicon document.
	Favicon bool
}

func (p *Pages) Dolly(w io.Writer, params DollyParams) error {
	return p.executePlain("fragments/dolly/logo", w, params)
}

func (p *Pages) Favicon(w io.Writer) error {
	return p.Dolly(w, DollyParams{
		Favicon: true,
	})
}

type LoginParams struct {
	BaseParams
	ReturnUrl  string
	ErrorCode  string
	AddAccount bool
	Handle     string
	Accounts   []oauth.AccountInfo
}

func (p *Pages) Login(w io.Writer, params LoginParams) error {
	return p.executeLogin("user/login", w, params)
}

type SignupParams struct {
	BaseParams
	CloudflareSiteKey string
	EmailId           string
}

func (p *Pages) Signup(w io.Writer, params SignupParams) error {
	return p.executeLogin("user/signup", w, params)
}

func (p *Pages) CompleteSignup(w io.Writer) error {
	return p.executeLogin("user/completeSignup", w, BaseParams{})
}

type SignupSuccessParams struct {
	Handle string
}

func (p *Pages) SignupSuccess(w io.Writer, params SignupSuccessParams) error {
	return p.executePlain("user/fragments/signupSuccess", w, params)
}

type TermsOfServiceParams struct {
	BaseParams
	Content template.HTML
}

func (p *Pages) TermsOfService(w io.Writer, params TermsOfServiceParams) error {
	filename := "terms.md"
	filePath := filepath.Join("legal", filename)

	file, err := p.embedFS.Open(filePath)
	if err != nil {
		return fmt.Errorf("failed to read %s: %w", filename, err)
	}
	defer file.Close()

	markdownBytes, err := io.ReadAll(file)
	if err != nil {
		return fmt.Errorf("failed to read %s: %w", filename, err)
	}

	rctx := p.rctx.Clone()
	rctx.RendererType = markup.RendererTypeDefault
	htmlString := rctx.RenderMarkdown(string(markdownBytes))
	sanitized := sanitizer.SanitizeDefault(htmlString)
	params.Content = template.HTML(sanitized)

	return p.execute("legal/terms", w, params)
}

type PrivacyPolicyParams struct {
	BaseParams
	Content template.HTML
}

func (p *Pages) PrivacyPolicy(w io.Writer, params PrivacyPolicyParams) error {
	filename := "privacy.md"
	filePath := filepath.Join("legal", filename)

	file, err := p.embedFS.Open(filePath)
	if err != nil {
		return fmt.Errorf("failed to read %s: %w", filename, err)
	}
	defer file.Close()

	markdownBytes, err := io.ReadAll(file)
	if err != nil {
		return fmt.Errorf("failed to read %s: %w", filename, err)
	}

	rctx := p.rctx.Clone()
	rctx.RendererType = markup.RendererTypeDefault
	htmlString := rctx.RenderMarkdown(string(markdownBytes))
	sanitized := sanitizer.SanitizeDefault(htmlString)
	params.Content = template.HTML(sanitized)

	return p.execute("legal/privacy", w, params)
}

type BrandParams struct {
	BaseParams
}

func (p *Pages) Brand(w io.Writer, params BrandParams) error {
	return p.execute("brand/brand", w, params)
}

type RecentItem struct {
	Link  *models.RecentLink
	Repo  *models.Repo
	Issue *models.Issue
	Pull  *models.Pull
}

type BlogPost struct {
	Slug     string
	Title    string
	Subtitle string
	Date     time.Time
}

type TimelineParams struct {
	BaseParams
	Onboarding       models.OnboardingProgress
	Timeline         []models.TimelineGroup
	Repos            []models.Repo
	GfiLabel         *models.LabelDefinition
	BlueskyPosts     []models.BskyPost
	VouchSuggestions []models.VouchSuggestion
	Notifications    []*models.NotificationWithEntity
	Recents          []RecentItem
	FollowingOnly    bool
	RecentBlogPosts  []BlogPost
	// ShowNewsletter controls whether the newsletter widget/CTA is rendered.
	// For logged-in users it reflects their newsletter_preferences row; for
	// anonymous visitors it is always true (dismissal falls back to
	// localStorage on the client).
	ShowNewsletter bool
	CanFocus       bool
}

func (p *Pages) Timeline(w io.Writer, params TimelineParams) error {
	return p.execute("timeline/timeline", w, params)
}

type GoodFirstIssuesParams struct {
	BaseParams
	Issues     []models.Issue
	RepoGroups []*models.RepoGroup
	LabelDefs  map[string]*models.LabelDefinition
	GfiLabel   *models.LabelDefinition
	Page       pagination.Page
}

func (p *Pages) GoodFirstIssues(w io.Writer, params GoodFirstIssuesParams) error {
	return p.execute("goodfirstissues/index", w, params)
}

type UserProfileSettingsParams struct {
	BaseParams
	Tab                 string
	PunchcardPreference models.PunchcardPreference
	IsTnglSh            bool
	IsDeactivated       bool
	HandleOpen          bool
}

func (p *Pages) UserProfileSettings(w io.Writer, params UserProfileSettingsParams) error {
	params.Tab = "profile"
	return p.execute("user/settings/profile", w, params)
}

type GroupedNotifications struct {
	Today    []*models.NotificationWithEntity
	ThisWeek []*models.NotificationWithEntity
	Older    []*models.NotificationWithEntity
}

func GroupNotificationsByDate(notifs []*models.NotificationWithEntity) GroupedNotifications {
	now := time.Now()
	todayStart := time.Date(now.Year(), now.Month(), now.Day(), 0, 0, 0, 0, now.Location())
	weekStart := todayStart.AddDate(0, 0, -6)

	var g GroupedNotifications
	for _, n := range notifs {
		switch {
		case !n.Created.Before(todayStart):
			g.Today = append(g.Today, n)
		case !n.Created.Before(weekStart):
			g.ThisWeek = append(g.ThisWeek, n)
		default:
			g.Older = append(g.Older, n)
		}
	}
	return g
}

type NotificationsParams struct {
	BaseParams
	WorkGroups        GroupedNotifications
	SocialGroups      GroupedNotifications
	MobileGroups      GroupedNotifications
	WorkUnreadCount   int64
	SocialUnreadCount int64
	Page              pagination.Page
	Total             int
	ReadFilter        string // "inbox" or "unread"
	CategoryFilter    string // "all", "work", "social"
	CanFocus          bool
}

func (p *Pages) Notifications(w io.Writer, params NotificationsParams) error {
	return p.execute("notifications/list", w, params)
}

func (p *Pages) NotificationItem(w io.Writer, notif *models.NotificationWithEntity) error {
	return p.executePlain("notifications/fragments/item", w, notif)
}

type NotificationCountParams struct {
	Count int64
}

func (p *Pages) NotificationCount(w io.Writer, params NotificationCountParams) error {
	return p.executePlain("notifications/fragments/count", w, params)
}

type NotificationPreviewParams struct {
	BaseParams
	Notifications  []*models.NotificationWithEntity
	ReadFilter     string
	CategoryFilter string
	CanFocus       bool
}

func (p *Pages) NotificationPreview(w io.Writer, params NotificationPreviewParams) error {
	return p.executePlain("notifications/fragments/preview", w, params)
}

type UserKeysSettingsParams struct {
	BaseParams
	PubKeys []models.PublicKey
	Tab     string
}

func (p *Pages) UserKeysSettings(w io.Writer, params UserKeysSettingsParams) error {
	params.Tab = "keys"
	return p.execute("user/settings/keys", w, params)
}

type UserEmailsSettingsParams struct {
	BaseParams
	Emails []models.Email
	Tab    string
}

func (p *Pages) UserEmailsSettings(w io.Writer, params UserEmailsSettingsParams) error {
	params.Tab = "emails"
	return p.execute("user/settings/emails", w, params)
}

type UserNotificationSettingsParams struct {
	BaseParams
	Preferences      *models.NotificationPreferences
	HasVerifiedEmail bool
	Tab              string
}

func (p *Pages) UserNotificationSettings(w io.Writer, params UserNotificationSettingsParams) error {
	params.Tab = "notifications"
	return p.execute("user/settings/notifications", w, params)
}

type UserSiteSettingsParams struct {
	BaseParams
	Claim        *models.DomainClaim
	SitesDomain  string
	IsTnglHandle bool
	Tab          string
}

func (p *Pages) UserSiteSettings(w io.Writer, params UserSiteSettingsParams) error {
	params.Tab = "sites"
	return p.execute("user/settings/sites", w, params)
}

type UpgradeBannerParams struct {
	Registrations []models.Registration
	Spindles      []models.Spindle
}

func (p *Pages) UpgradeBanner(w io.Writer, params UpgradeBannerParams) error {
	return p.executePlain("banner", w, params)
}

type NewsletterResponseParams struct {
	// Id identifies the calling form instance; the response span's id will
	// be "newsletter-msg-<Id>" so it round-trips with the form's hx-target.
	Id string
	// Error, when non-empty, switches the template to the error variant.
	Error string
}

func (p *Pages) NewsletterResponse(w io.Writer, params NewsletterResponseParams) error {
	return p.executePlain("timeline/fragments/newsletterResponse", w, params)
}

type KnotsParams struct {
	BaseParams
	Knots []KnotListingParams
	Tab   string
}

func (p *Pages) Knots(w io.Writer, params KnotsParams) error {
	params.Tab = "knots"
	return p.execute("knots/index", w, params)
}

type KnotParams struct {
	BaseParams
	Registration *models.Registration
	Members      []string
	Repos        map[string][]models.Repo
	IsOwner      bool
	RepoCount    int
	Tab          string
}

func (p *Pages) Knot(w io.Writer, params KnotParams) error {
	return p.execute("knots/dashboard", w, params)
}

type KnotListingParams struct {
	*models.Registration
	RepoCount int
}

func (p *Pages) KnotListing(w io.Writer, params KnotListingParams) error {
	return p.executePlain("knots/fragments/knotListing", w, params)
}

type SpindlesParams struct {
	BaseParams
	Spindles []models.Spindle
	Tab      string
}

func (p *Pages) Spindles(w io.Writer, params SpindlesParams) error {
	params.Tab = "spindles"
	return p.execute("spindles/index", w, params)
}

type SpindleListingParams struct {
	models.Spindle
	Tab string
}

func (p *Pages) SpindleListing(w io.Writer, params SpindleListingParams) error {
	return p.executePlain("spindles/fragments/spindleListing", w, params)
}

type SpindleDashboardParams struct {
	BaseParams
	Spindle models.Spindle
	Members []string
	Repos   map[string][]models.Repo
	Tab     string
}

func (p *Pages) SpindleDashboard(w io.Writer, params SpindleDashboardParams) error {
	return p.execute("spindles/dashboard", w, params)
}

type NewRepoParams struct {
	BaseParams
	Knots    []string
	Spindles []string
}

func (p *Pages) NewRepo(w io.Writer, params NewRepoParams) error {
	return p.execute("repo/new", w, params)
}

type ForkRepoParams struct {
	BaseParams
	Knots    []string
	Spindles []string
	RepoInfo repoinfo.RepoInfo
}

func (p *Pages) ForkRepo(w io.Writer, params ForkRepoParams) error {
	return p.execute("repo/fork", w, params)
}

type ProfileCard struct {
	UserDid           string
	HasProfile        bool
	IsTangledUser     bool
	FollowStatus      models.FollowStatus
	VouchRelationship *models.VouchRelationship
	Punchcard         *models.Punchcard
	Profile           *models.Profile
	Stats             ProfileStats
	Active            string
	ProfileScript     string
}

type ProfileStats struct {
	RepoCount      int64
	StarredCount   int64
	StringCount    int64
	FollowersCount int64
	FollowingCount int64
}

func (p *ProfileCard) GetTabs() [][]any {
	tabs := [][]any{
		{"overview", "overview", "square-chart-gantt", nil},
		{"repos", "repos", "book-marked", p.Stats.RepoCount},
		{"starred", "starred", "star", p.Stats.StarredCount},
		{"strings", "strings", "line-squiggle", p.Stats.StringCount},
		{"vouches", "vouches", "shield", nil},
	}

	return tabs
}

type ProfileOverviewParams struct {
	BaseParams
	Repos              []models.Repo
	CollaboratingRepos []models.Repo
	ProfileTimeline    *models.ProfileTimeline
	Card               *ProfileCard
	Active             string
	ShowPunchcard      bool
}

func (p *Pages) ProfileOverview(w io.Writer, params ProfileOverviewParams) error {
	params.Active = "overview"
	return p.executeProfile("user/overview", w, params)
}

type ProfileReposParams struct {
	BaseParams
	Repos        []models.Repo
	StarStatuses map[string]bool
	Card         *ProfileCard
	Active       string
	Page         pagination.Page
	RepoCount    int
	FilterQuery  string
}

func (p *Pages) ProfileRepos(w io.Writer, params ProfileReposParams) error {
	params.Active = "repos"
	return p.executeProfile("user/repos", w, params)
}

type ProfileStarredParams struct {
	BaseParams
	Repos  []models.Repo
	Card   *ProfileCard
	Page   pagination.Page
	Total  int
	Active string
}

func (p *Pages) ProfileStarred(w io.Writer, params ProfileStarredParams) error {
	params.Active = "starred"
	return p.executeProfile("user/starred", w, params)
}

type ProfileStringsParams struct {
	BaseParams
	Strings []models.String
	Card    *ProfileCard
	Active  string
}

func (p *Pages) ProfileStrings(w io.Writer, params ProfileStringsParams) error {
	params.Active = "strings"
	return p.executeProfile("user/strings", w, params)
}

type ProfileVouchesParams struct {
	BaseParams
	Vouches        []models.Vouch
	Suggestions    []models.VouchSuggestion
	Card           *ProfileCard
	Page           pagination.Page
	VouchCount     int
	Active         string
	EvidencePulls  map[syntax.ATURI]*models.Pull
	EvidenceIssues map[syntax.ATURI]*models.Issue
}

func (p *Pages) ProfileVouches(w io.Writer, params ProfileVouchesParams) error {
	params.Active = "vouches"
	return p.executeProfile("user/vouches", w, params)
}

type FollowCard struct {
	UserDid string
	BaseParams
	FollowStatus   models.FollowStatus
	FollowersCount int64
	FollowingCount int64
	Profile        *models.Profile
}

type ProfileFollowersParams struct {
	BaseParams
	Followers []FollowCard
	Card      *ProfileCard
	Active    string
}

func (p *Pages) ProfileFollowers(w io.Writer, params ProfileFollowersParams) error {
	params.Active = "overview"
	return p.executeProfile("user/followers", w, params)
}

type ProfileFollowingParams struct {
	BaseParams
	Following []FollowCard
	Card      *ProfileCard
	Active    string
}

func (p *Pages) ProfileFollowing(w io.Writer, params ProfileFollowingParams) error {
	params.Active = "overview"
	return p.executeProfile("user/following", w, params)
}

type FollowFragmentParams struct {
	UserDid        string
	FollowStatus   models.FollowStatus
	FollowersCount int64
	HxSwapOob      struct{} // empty struct so always truthy
}

func (p *Pages) FollowFragment(w io.Writer, params FollowFragmentParams) error {
	return p.executePlain("user/fragments/follow-oob", w, params)
}

type ProfilePopoverParams struct {
	BaseParams
	UserDid           string
	Profile           *models.Profile
	FollowStatus      models.FollowStatus
	VouchRelationship *models.VouchRelationship
	Stats             ProfilePopoverStats
}

type ProfilePopoverStats struct {
	FollowersCount int64
	FollowingCount int64
}

func (p *Pages) ProfilePopoverFragment(w io.Writer, params ProfilePopoverParams) error {
	return p.executePlain("user/fragments/profilePopover", w, params)
}

type EditBioParams struct {
	BaseParams
	Profile     *models.Profile
	AlsoKnownAs []string
	// Action optionally overrides the form's hx-post target. Defaults to
	// /profile/bio when empty. Used by the onboarding flow to save + advance.
	Action string
}

func (p *Pages) EditBioFragment(w io.Writer, params EditBioParams) error {
	return p.executePlain("user/fragments/editBio", w, params)
}

type EditPinsParams struct {
	BaseParams
	Profile  *models.Profile
	AllRepos []PinnedRepo
}

type PinnedRepo struct {
	IsPinned bool
	models.Repo
}

func (p *Pages) EditPinsFragment(w io.Writer, params EditPinsParams) error {
	return p.executePlain("user/fragments/editPins", w, params)
}

type StarBtnFragmentParams struct {
	IsStarred bool
	SubjectAt syntax.ATURI
	StarCount int
	RepoName  string
	HxSwapOob bool
}

func (p *Pages) StarBtnFragment(w io.Writer, params StarBtnFragmentParams) error {
	params.HxSwapOob = true
	return p.executePlain("fragments/starBtn", w, params)
}

type OnboardingParams struct {
	BaseParams
	Step int

	EditBio EditBioParams

	PubKeys []models.PublicKey

	People        []FollowCard
	TrendingRepos []models.Repo
	StarStatuses  map[string]bool
}

// KeyFragment renders a single SSH key row (used to append a newly added key
// to the list without a full reload).
func (p *Pages) KeyFragment(w io.Writer, key models.PublicKey) error {
	return p.executePlain("user/settings/fragments/keyListing", w, key)
}

func (p *Pages) Onboarding(w io.Writer, params OnboardingParams) error {
	return p.executeOnboarding("onboarding/welcome", w, params)
}

type RepoIndexParams struct {
	BaseParams
	RepoInfo      repoinfo.RepoInfo
	Active        string
	TagMap        map[string][]string
	CommitsTrunc  []types.Commit
	TagsTrunc     []*types.TagReference
	BranchesTrunc []types.Branch
	// ForkInfo           *types.ForkInfo
	HTMLReadme       template.HTML
	Raw              bool
	EmailToDid       map[string]string
	VerifiedCommits  commitverify.VerifiedCommits
	Languages        []types.RepoLanguageDetails
	NeedsKnotUpgrade bool
	KnotUnreachable  bool
	types.RepoIndexResponse
}

func (p *Pages) RepoIndexPage(w io.Writer, params RepoIndexParams) error {
	params.Active = "overview"
	if params.IsEmpty {
		return p.executeRepo("repo/empty", w, params)
	}

	if params.NeedsKnotUpgrade {
		return p.executeRepo("repo/needsUpgrade", w, params)
	}

	if params.KnotUnreachable {
		return p.executeRepo("repo/knotUnreachable", w, params)
	}

	rctx := p.rctx.Clone()
	rctx.RepoInfo = params.RepoInfo
	rctx.RepoInfo.Ref = params.Ref
	rctx.RendererType = markup.RendererTypeRepoMarkdown

	if params.ReadmeFileName != "" {
		switch markup.GetFormat(params.ReadmeFileName) {
		case markup.FormatMarkdown:
			params.Raw = false
			htmlString := rctx.RenderMarkdown(params.Readme)
			sanitized := sanitizer.SanitizeDefault(htmlString)
			params.HTMLReadme = template.HTML(sanitized)
		default:
			params.Raw = true
		}
	}

	return p.executeRepo("repo/index", w, params)
}

type RepoSearchParams struct {
	BaseParams
	RepoInfo    repoinfo.RepoInfo
	Active      string
	FilterQuery string
}

func (p *Pages) RepoSearchPage(w io.Writer, params RepoSearchParams) error {
	params.Active = "overview"
	return p.executeRepo("repo/search", w, params)
}

type RepoSearchResultsFragmentParams struct {
	Query    string
	Results  []SearchResult
	ErrorMsg string
}

func (p *Pages) RepoSearchResultsFragment(w io.Writer, params RepoSearchResultsFragmentParams) error {
	return p.executePlain("repo/fragments/searchResults", w, params)
}

type RepoLogParams struct {
	BaseParams
	RepoInfo        repoinfo.RepoInfo
	TagMap          map[string][]string
	Active          string
	EmailToDid      map[string]string
	VerifiedCommits commitverify.VerifiedCommits

	types.RepoLogResponse
}

func (p *Pages) RepoLog(w io.Writer, params RepoLogParams) error {
	params.Active = "overview"
	return p.executeRepo("repo/log", w, params)
}

type RepoCommitParams struct {
	BaseParams
	RepoInfo   repoinfo.RepoInfo
	Active     string
	EmailToDid map[string]string
	Pipeline   *types.Pipeline
	DiffOpts   types.DiffOpts

	// singular because it's always going to be just one
	VerifiedCommit commitverify.VerifiedCommits

	types.RepoCommitResponse
}

func (p *Pages) RepoCommit(w io.Writer, params RepoCommitParams) error {
	params.Active = "overview"
	return p.executeRepo("repo/commit", w, params)
}

type RepoTreeParams struct {
	BaseParams
	RepoInfo       repoinfo.RepoInfo
	Active         string
	BreadCrumbs    [][]string
	Path           string
	Raw            bool
	HTMLReadme     template.HTML
	EmailToDid     map[string]string
	LastCommitInfo *types.LastCommitInfo
	Ref            string
	Parent         string
	DotDot         string
	Files          []types.NiceTree
	ReadmeFileName string
	Readme         string
}

type RepoTreeStats struct {
	NumFolders uint64
	NumFiles   uint64
}

func (r RepoTreeParams) TreeStats() RepoTreeStats {
	numFolders, numFiles := 0, 0
	for _, f := range r.Files {
		if !f.IsFile() {
			numFolders += 1
		} else if f.IsFile() {
			numFiles += 1
		}
	}

	return RepoTreeStats{
		NumFolders: uint64(numFolders),
		NumFiles:   uint64(numFiles),
	}
}

func (p *Pages) RepoTree(w io.Writer, params RepoTreeParams) error {
	params.Active = "overview"

	rctx := p.rctx.Clone()
	rctx.RepoInfo = params.RepoInfo
	rctx.RepoInfo.Ref = params.Ref
	rctx.RendererType = markup.RendererTypeRepoMarkdown

	if params.ReadmeFileName != "" {
		switch markup.GetFormat(params.ReadmeFileName) {
		case markup.FormatMarkdown:
			params.Raw = false
			htmlString := rctx.RenderMarkdown(params.Readme)
			sanitized := sanitizer.SanitizeDefault(htmlString)
			params.HTMLReadme = template.HTML(sanitized)
		default:
			params.Raw = true
		}
	}

	return p.executeRepo("repo/tree", w, params)
}

type RepoBranchesParams struct {
	BaseParams
	RepoInfo repoinfo.RepoInfo
	Active   string
	types.RepoBranchesResponse
}

func (p *Pages) RepoBranches(w io.Writer, params RepoBranchesParams) error {
	params.Active = "overview"
	return p.executeRepo("repo/branches", w, params)
}

type RepoTagsParams struct {
	BaseParams
	RepoInfo repoinfo.RepoInfo
	Active   string
	types.RepoTagsResponse
	ArtifactMap       map[plumbing.Hash][]models.Artifact
	DanglingArtifacts []models.Artifact
}

func (p *Pages) RepoTags(w io.Writer, params RepoTagsParams) error {
	params.Active = "overview"
	return p.executeRepo("repo/tags", w, params)
}

type RepoTagParams struct {
	BaseParams
	RepoInfo repoinfo.RepoInfo
	Active   string
	types.RepoTagResponse
	ArtifactMap       map[plumbing.Hash][]models.Artifact
	DanglingArtifacts []models.Artifact
}

func (p *Pages) RepoTag(w io.Writer, params RepoTagParams) error {
	params.Active = "overview"
	return p.executeRepo("repo/tag", w, params)
}

type RepoArtifactParams struct {
	BaseParams
	RepoInfo repoinfo.RepoInfo
	Artifact models.Artifact
}

func (p *Pages) RepoArtifactFragment(w io.Writer, params RepoArtifactParams) error {
	return p.executePlain("repo/fragments/artifact", w, params)
}

type RepoBlobParams struct {
	BaseParams
	RepoInfo       repoinfo.RepoInfo
	Active         string // always "overview"
	BreadCrumbs    [][]string
	BlobView       models.BlobView // TODO: expose this struct
	ShowRendered   bool
	EmailToDid     map[string]string
	LastCommitInfo *types.LastCommitInfo
	Ref            string
	Path           string
	Language       string
}

func (p *Pages) RepoBlob(w io.Writer, params RepoBlobParams) error {
	params.Active = "overview"
	return p.executeRepo("repo/blob", w, params)
}

type Collaborator struct {
	Did  string
	Role string
}

type RepoSettingsParams struct {
	BaseParams
	RepoInfo       repoinfo.RepoInfo
	Collaborators  []Collaborator
	Active         string
	Branches       []types.Branch
	Spindles       []string
	CurrentSpindle string
	Secrets        []*tangled.RepoListSecrets_Secret

	// TODO: use repoinfo.roles
	IsCollaboratorInviteAllowed bool
}

func (p *Pages) RepoSettings(w io.Writer, params RepoSettingsParams) error {
	params.Active = "settings"
	return p.executeRepo("repo/settings", w, params)
}

type RepoGeneralSettingsParams struct {
	BaseParams
	RepoInfo           repoinfo.RepoInfo
	Labels             []models.LabelDefinition
	DefaultLabels      []models.LabelDefinition
	SubscribedLabels   map[string]struct{}
	ShouldSubscribeAll bool
	Active             string
	Tab                string
	Branches           []types.Branch
}

func (p *Pages) RepoGeneralSettings(w io.Writer, params RepoGeneralSettingsParams) error {
	params.Active = "settings"
	params.Tab = "general"
	return p.executeRepo("repo/settings/general", w, params)
}

type RepoAccessSettingsParams struct {
	BaseParams
	RepoInfo              repoinfo.RepoInfo
	Active                string
	Tab                   string
	Collaborators         []Collaborator
	CanRemoveCollaborator bool
}

func (p *Pages) RepoAccessSettings(w io.Writer, params RepoAccessSettingsParams) error {
	params.Active = "settings"
	params.Tab = "access"
	return p.executeRepo("repo/settings/access", w, params)
}

type RepoPipelineSettingsParams struct {
	BaseParams
	RepoInfo       repoinfo.RepoInfo
	Active         string
	Tab            string
	Spindles       []string
	CurrentSpindle string
	Secrets        []map[string]any
}

func (p *Pages) RepoPipelineSettings(w io.Writer, params RepoPipelineSettingsParams) error {
	params.Active = "settings"
	params.Tab = "pipelines"
	return p.executeRepo("repo/settings/pipelines", w, params)
}

type RepoWebhooksSettingsParams struct {
	BaseParams
	RepoInfo          repoinfo.RepoInfo
	Active            string
	Tab               string
	Webhooks          []models.Webhook
	WebhookDeliveries map[int64][]models.WebhookDelivery
}

func (p *Pages) RepoWebhooksSettings(w io.Writer, params RepoWebhooksSettingsParams) error {
	params.Active = "settings"
	params.Tab = "hooks"
	return p.executeRepo("repo/settings/hooks", w, params)
}

type WebhookDeliveriesListParams struct {
	BaseParams
	RepoInfo   repoinfo.RepoInfo
	Webhook    *models.Webhook
	Deliveries []models.WebhookDelivery
}

func (p *Pages) WebhookDeliveriesList(w io.Writer, params WebhookDeliveriesListParams) error {
	tpl, err := p.parse("repo/settings/fragments/webhookDeliveries")
	if err != nil {
		return err
	}
	return tpl.ExecuteTemplate(w, "repo/settings/fragments/webhookDeliveries", params)
}

type RepoSiteSettingsParams struct {
	BaseParams
	RepoInfo         repoinfo.RepoInfo
	Active           string
	Tab              string
	Branches         []types.Branch
	SiteConfig       *models.RepoSite
	OwnerClaim       *models.DomainClaim
	Deploys          []models.SiteDeploy
	IndexSiteTakenBy string // repo_at of another repo that already holds is_index, or ""
}

func (p *Pages) RepoSiteSettings(w io.Writer, params RepoSiteSettingsParams) error {
	params.Active = "settings"
	params.Tab = "sites"
	return p.executeRepo("repo/settings/sites", w, params)
}

type RepoIssuesParams struct {
	BaseParams
	RepoInfo           repoinfo.RepoInfo
	Active             string
	Issues             []models.Issue
	IssueCount         int
	LabelDefs          map[string]*models.LabelDefinition
	Page               pagination.Page
	FilterState        string
	FilterQuery        string
	BaseFilterQuery    string
	VouchRelationships map[syntax.DID]*models.VouchRelationship
}

func (p *Pages) RepoIssues(w io.Writer, params RepoIssuesParams) error {
	params.Active = "issues"
	return p.executeRepo("repo/issues/issues", w, params)
}

type RepoSingleIssueParams struct {
	BaseParams
	RepoInfo    repoinfo.RepoInfo
	Active      string
	Issue       *models.Issue
	CommentList []models.CommentListItem
	Backlinks   []models.RichReferenceLink
	LabelDefs   map[string]*models.LabelDefinition

	Reactions          map[syntax.ATURI]map[models.ReactionKind]models.ReactionDisplayData
	UserReacted        map[syntax.ATURI]map[models.ReactionKind]bool
	VouchRelationships map[syntax.DID]*models.VouchRelationship

	// IsSubscribed is nil when the user is not logged in, true when subscribed,
	// false when explicitly unsubscribed, and nil when no explicit subscription.
	IsSubscribed *bool
}

func (p *Pages) RepoSingleIssue(w io.Writer, params RepoSingleIssueParams) error {
	params.Active = "issues"
	return p.executeRepo("repo/issues/issue", w, params)
}

type IssueSubscribeParams struct {
	RepoInfo     repoinfo.RepoInfo
	IssueId      int
	IsSubscribed *bool
}

func (p *Pages) IssueSubscribeFragment(w io.Writer, params IssueSubscribeParams) error {
	return p.executePlain("repo/issues/fragments/subscribeButton", w, params)
}

type PullSubscribeParams struct {
	RepoInfo     repoinfo.RepoInfo
	PullId       int
	IsSubscribed *bool
}

func (p *Pages) PullSubscribeFragment(w io.Writer, params PullSubscribeParams) error {
	return p.executePlain("repo/pulls/fragments/subscribeButton", w, params)
}

type EditIssueParams struct {
	BaseParams
	RepoInfo repoinfo.RepoInfo
	Issue    *models.Issue
	Action   string
}

func (p *Pages) EditIssueFragment(w io.Writer, params EditIssueParams) error {
	params.Action = "edit"
	return p.executePlain("repo/issues/fragments/putIssue", w, params)
}

type ThreadReactionFragmentParams struct {
	Kind        models.ReactionKind
	Count       int
	Users       []string
	IsReacted   bool
	CommentRkey string
	SubjectUri  string
}

func (p *Pages) ThreadReactionFragment(w io.Writer, params ThreadReactionFragmentParams) error {
	return p.executePlain("repo/fragments/reaction", w, params)
}

type RepoNewIssueParams struct {
	BaseParams
	RepoInfo repoinfo.RepoInfo
	Issue    *models.Issue // existing issue if any -- passed when editing
	Active   string
	Action   string
}

func (p *Pages) RepoNewIssue(w io.Writer, params RepoNewIssueParams) error {
	params.Active = "issues"
	params.Action = "create"
	return p.executeRepo("repo/issues/new", w, params)
}

type StackedDiff struct {
	Diff *types.NiceDiff
	Opts types.DiffOpts
}

type RepoNewPullParams struct {
	BaseParams
	RepoInfo         repoinfo.RepoInfo
	Branches         []types.Branch
	SourceBranches   []types.Branch
	ForkBranches     []types.Branch
	Forks            []models.Repo
	Source           Source
	SourceBranch     string
	TargetBranch     string
	Fork             string
	Patch            string
	Title            string
	Body             string
	TitleDirty       bool
	BodyDirty        bool
	IsStacked        bool
	Comparison       *types.RepoFormatPatchResponse
	Diff             *types.NiceDiff
	DiffOpts         types.DiffOpts
	StackedDiffs     []StackedDiff
	MergeCheck       *types.MergeCheckResponse
	StackTitles      map[string]string
	StackBodies      map[string]string
	PrefillError     string
	Active           string
	LabelDefs        map[string]*models.LabelDefinition
	LabelState       models.LabelState
	StackLabelStates map[string]models.LabelState
}

func (p *Pages) RepoNewPull(w io.Writer, params RepoNewPullParams) error {
	params.Active = "pulls"
	return p.executeRepo("repo/pulls/new", w, params)
}

func (p *Pages) PullComposeHostFragment(w io.Writer, params RepoNewPullParams) error {
	return p.executePlain("repo/pulls/fragments/pullComposeHost", w, params)
}

func (p *Pages) MarkdownPreviewFragment(w io.Writer, body string) error {
	return p.executePlain("fragments/markdownPreview", w, body)
}

type EditPullParams struct {
	LoggedInUser *oauth.MultiAccountUser
	RepoInfo     repoinfo.RepoInfo
	Pull         *models.Pull
}

func (p *Pages) EditPullFragment(w io.Writer, params EditPullParams) error {
	return p.executePlain("repo/pulls/fragments/pullEdit", w, params)
}

type RepoPullsParams struct {
	BaseParams
	RepoInfo           repoinfo.RepoInfo
	Pulls              []*models.Pull
	Active             string
	FilterState        string
	FilterQuery        string
	BaseFilterQuery    string
	Stacks             []models.Stack
	Pipelines          map[string]types.Pipeline
	LabelDefs          map[string]*models.LabelDefinition
	Page               pagination.Page
	PullCount          int
	VouchRelationships map[syntax.DID]*models.VouchRelationship
}

func (p *Pages) RepoPulls(w io.Writer, params RepoPullsParams) error {
	params.Active = "pulls"
	return p.executeRepo("repo/pulls/pulls", w, params)
}

type ResubmitResult uint64

const (
	ShouldResubmit ResubmitResult = iota
	ShouldNotResubmit
	Unknown
)

func (r ResubmitResult) Yes() bool {
	return r == ShouldResubmit
}
func (r ResubmitResult) No() bool {
	return r == ShouldNotResubmit
}
func (r ResubmitResult) Unknown() bool {
	return r == Unknown
}

type RepoSinglePullParams struct {
	BaseParams
	RepoInfo           repoinfo.RepoInfo
	Active             string
	Pull               *models.Pull
	Stack              models.Stack
	Backlinks          []models.RichReferenceLink
	BranchDeleteStatus *models.BranchDeleteStatus
	MergeCheck         types.MergeCheckResponse
	ResubmitCheck      ResubmitResult
	Pipelines          map[string]types.Pipeline
	Diff               types.DiffRenderer
	DiffOpts           types.DiffOpts
	ActiveRound        int
	IsInterdiff        bool

	Reactions   map[syntax.ATURI]map[models.ReactionKind]models.ReactionDisplayData
	UserReacted map[syntax.ATURI]map[models.ReactionKind]bool

	LabelDefs          map[string]*models.LabelDefinition
	VouchRelationships map[syntax.DID]*models.VouchRelationship
	VouchSkips         map[syntax.DID]bool

	// IsSubscribed is nil when not logged in, true when subscribed, false when explicitly unsubscribed.
	IsSubscribed *bool
}

func (p *Pages) RepoSinglePull(w io.Writer, params RepoSinglePullParams) error {
	params.Active = "pulls"
	return p.executeRepo("repo/pulls/pull", w, params)
}

type PullResubmitParams struct {
	BaseParams
	RepoInfo     repoinfo.RepoInfo
	Pull         *models.Pull
	SubmissionId int
}

func (p *Pages) PullResubmitFragment(w io.Writer, params PullResubmitParams) error {
	return p.executePlain("repo/pulls/fragments/pullResubmit", w, params)
}

type PullActionsParams struct {
	BaseParams
	RepoInfo           repoinfo.RepoInfo
	Pull               *models.Pull
	RoundNumber        int
	MergeCheck         types.MergeCheckResponse
	ResubmitCheck      ResubmitResult
	BranchDeleteStatus *models.BranchDeleteStatus
	Stack              models.Stack

	// Workflow warning state for fork-based pulls without a pipeline on the
	// latest commit. WorkflowsChanged and ChangedWorkflowFiles are computed
	// from the latest round's patch.
	WorkflowsChanged     bool
	ChangedWorkflowFiles []string
	HasPipeline          bool

	// renders buttons in a pre-check state and attaches the hx-trigger="load"
	// that fetches the real, checked fragment
	Loading bool
}

func (p *Pages) PullActionsFragment(w io.Writer, params PullActionsParams) error {
	return p.executePlain("repo/pulls/fragments/pullActions", w, params)
}

type RepoCompareParams struct {
	BaseParams
	RepoInfo repoinfo.RepoInfo
	Forks    []models.Repo
	Branches []types.Branch
	Tags     []*types.TagReference
	Base     string
	Head     string
	Diff     *types.NiceDiff
	DiffOpts types.DiffOpts

	Active string
}

func (p *Pages) RepoCompare(w io.Writer, params RepoCompareParams) error {
	params.Active = "overview"
	return p.executeRepo("repo/compare/compare", w, params)
}

type RepoCompareNewParams struct {
	BaseParams
	RepoInfo repoinfo.RepoInfo
	Forks    []models.Repo
	Branches []types.Branch
	Tags     []*types.TagReference
	Base     string
	Head     string

	Active string
}

func (p *Pages) RepoCompareNew(w io.Writer, params RepoCompareNewParams) error {
	params.Active = "overview"
	return p.executeRepo("repo/compare/new", w, params)
}

type RepoCompareAllowPullParams struct {
	BaseParams
	RepoInfo repoinfo.RepoInfo
	Base     string
	Head     string
}

func (p *Pages) RepoCompareAllowPullFragment(w io.Writer, params RepoCompareAllowPullParams) error {
	return p.executePlain("repo/fragments/compareAllowPull", w, params)
}

type RepoCompareDiffFragmentParams struct {
	Diff     types.NiceDiff
	DiffOpts types.DiffOpts
}

func (p *Pages) RepoCompareDiffFragment(w io.Writer, params RepoCompareDiffFragmentParams) error {
	return p.executePlain("repo/fragments/diff", w, []any{&params.Diff, &params.DiffOpts})
}

type LabelPanelParams struct {
	BaseParams
	RepoInfo repoinfo.RepoInfo
	Defs     map[string]*models.LabelDefinition
	Subject  string
	State    models.LabelState
}

func (p *Pages) LabelPanel(w io.Writer, params LabelPanelParams) error {
	return p.executePlain("repo/fragments/labelPanel", w, params)
}

type EditLabelPanelParams struct {
	BaseParams
	RepoInfo repoinfo.RepoInfo
	Defs     map[string]*models.LabelDefinition
	Subject  string
	State    models.LabelState
	Prefix   string
}

func (p *Pages) EditLabelPanel(w io.Writer, params EditLabelPanelParams) error {
	return p.executePlain("repo/fragments/editLabelPanel", w, params)
}

type RepoStarsParams struct {
	BaseParams
	RepoInfo   repoinfo.RepoInfo
	Active     string
	Starrers   []models.Star
	Page       pagination.Page
	TotalCount int
}

func (p *Pages) RepoStars(w io.Writer, params RepoStarsParams) error {
	params.Active = "overview"
	return p.executeRepo("repo/stars", w, params)
}

type RepoForksParams struct {
	BaseParams
	RepoInfo   repoinfo.RepoInfo
	Active     string
	Forks      []models.Repo
	Page       pagination.Page
	TotalCount int
}

func (p *Pages) RepoForks(w io.Writer, params RepoForksParams) error {
	params.Active = "overview"
	return p.executeRepo("repo/forks", w, params)
}

type PipelinesParams struct {
	BaseParams
	RepoInfo   repoinfo.RepoInfo
	Pipelines  []types.Pipeline
	Active     string
	FilterKind string
	Total      int64
}

func (p *Pages) Pipelines(w io.Writer, params PipelinesParams) error {
	params.Active = "pipelines"
	return p.executeRepo("repo/pipelines/pipelines", w, params)
}

type LogBlockParams struct {
	Id        int
	Name      string
	Command   string
	Collapsed bool
	StartTime time.Time
}

func (p *Pages) LogBlock(w io.Writer, params LogBlockParams) error {
	return p.executePlain("repo/pipelines/fragments/logBlock", w, params)
}

type LogBlockEndParams struct {
	Id        int
	StartTime time.Time
	EndTime   time.Time
}

func (p *Pages) LogBlockEnd(w io.Writer, params LogBlockEndParams) error {
	return p.executePlain("repo/pipelines/fragments/logBlockEnd", w, params)
}

type LogLineParams struct {
	Id      int
	Content template.HTML
}

func (p *Pages) LogLine(w io.Writer, params LogLineParams) error {
	return p.executePlain("repo/pipelines/fragments/logLine", w, params)
}

type WorkflowSymbolOOBParams struct {
	Name     string
	Statuses models.WorkflowStatus
}

func (p *Pages) WorkflowSymbolOOB(w io.Writer, params WorkflowSymbolOOBParams) error {
	return p.executePlain("repo/pipelines/fragments/workflowSymbolOOB", w, params)
}

type PipelineStatusesParams struct {
	RepoInfo  repoinfo.RepoInfo
	Pipelines map[string]types.Pipeline
}

func (p *Pages) PipelineStatusesFragment(w io.Writer, params PipelineStatusesParams) error {
	return p.executePlain("repo/fragments/pipelineStatuses", w, params)
}

type WorkflowParams struct {
	BaseParams
	RepoInfo      repoinfo.RepoInfo
	Pipeline      types.Pipeline
	Workflow      string
	SSHLogCommand string
	Active        string
}

func (p *Pages) Workflow(w io.Writer, params WorkflowParams) error {
	params.Active = "pipelines"
	return p.executeRepo("repo/pipelines/workflow", w, params)
}

type PutStringParams struct {
	BaseParams
	Action string

	// this is supplied in the case of editing an existing string
	String models.String
}

func (p *Pages) PutString(w io.Writer, params PutStringParams) error {
	return p.execute("strings/put", w, params)
}

type StringsDashboardParams struct {
	BaseParams
	Card    ProfileCard
	Strings []models.String
}

func (p *Pages) StringsDashboard(w io.Writer, params StringsDashboardParams) error {
	return p.execute("strings/dashboard", w, params)
}

type StringTimelineParams struct {
	BaseParams
	Strings []models.String
}

func (p *Pages) StringsTimeline(w io.Writer, params StringTimelineParams) error {
	return p.execute("strings/timeline", w, params)
}

type SingleStringParams struct {
	BaseParams
	ShowRendered     bool
	RenderToggle     bool
	RenderedContents template.HTML
	String           *models.String
	Stats            models.StringStats
	IsStarred        bool
	StarCount        int
	Owner            identity.Identity
	CommentList      []models.CommentListItem

	Reactions          map[syntax.ATURI]map[models.ReactionKind]models.ReactionDisplayData
	UserReacted        map[syntax.ATURI]map[models.ReactionKind]bool
	VouchRelationships map[syntax.DID]*models.VouchRelationship
}

func (p *Pages) SingleString(w io.Writer, params SingleStringParams) error {
	return p.execute("strings/string", w, params)
}

type SearchReposParams struct {
	BaseParams
	FilterType  string // "repo" | "code"
	Repos       []SearchResult
	Page        pagination.Page
	ResultCount int
	FilterQuery string
	SortParam   string
	TimeTaken   time.Duration
	DocCount    int64
	ErrorMsg    string
}

func (p *Pages) SearchRepos(w io.Writer, params SearchReposParams) error {
	params.FilterType = "repo"
	return p.execute("search/search", w, params)
}

type SearchQuickParams struct {
	Repos []models.Repo
	Query string
	Total int
}

func (p *Pages) SearchQuick(w io.Writer, params SearchQuickParams) error {
	return p.executePlain("search/fragments/quick", w, params)
}

func (p *Pages) SearchQuickMobile(w io.Writer, params SearchQuickParams) error {
	tpl, err := p.parse("search/fragments/quick")
	if err != nil {
		return err
	}
	return tpl.ExecuteTemplate(w, "search/fragments/quickMobile", params)
}

type SearchResult struct {
	RepoDID  syntax.DID
	Repo     *models.Repo
	FilePath string
	Branches []string
	Commit   string
	Language string

	File   *CodeSearchResult_File  // filename match
	Chunks CodeSearchResult_Chunks // content matches
}

// CodeSearchResult_Chunk is a content match with its lines pre-rendered.
type CodeSearchResult_Chunk struct {
	Lines      []ChunkLine // precomputed from Content/ContentStartLine/Ranges
	MatchCount int         // number of match ranges in this chunk
}

type CodeSearchResult_Chunks []CodeSearchResult_Chunk

func (cs CodeSearchResult_Chunks) MatchCount() int {
	count := 0
	for _, c := range cs {
		count += c.MatchCount
	}
	return count
}

type CodeSearchResult_File struct {
	NameSpans []ChunkSpan // precomputed from FilePath/Ranges
}

type ChunkSpan struct {
	Text  string
	Match bool
}

type ChunkLine struct {
	Num       int
	Spans     []ChunkSpan
	Highlight bool
}

// ChunkLines renders a chunk's Content into per-line ChunkLines, splitting each
// line into matched/unmatched spans using ranges. startLine is the 1-based line
// number of the first line.
func ChunkLines(content string, startLine int, ranges []zoekt.Range) []ChunkLine {
	if startLine < 1 {
		startLine = 1
	}
	// trim a single trailing newline so we don't emit a spurious empty line
	content = strings.TrimSuffix(content, "\n")
	lines := strings.Split(content, "\n")
	out := make([]ChunkLine, len(lines))
	for i, text := range lines {
		num := startLine + i
		runes := []rune(text)

		// collect matched rune intervals [c0,c1) for this line
		var intervals [][2]int
		for _, rg := range ranges {
			if num < int(rg.Start.LineNumber) || num > int(rg.End.LineNumber) {
				continue
			}
			c0, c1 := 0, len(runes)
			if num == int(rg.Start.LineNumber) {
				c0 = int(rg.Start.Column) - 1
			}
			if num == int(rg.End.LineNumber) {
				c1 = int(rg.End.Column) - 1
			}
			c0 = max(0, min(c0, len(runes)))
			c1 = max(0, min(c1, len(runes)))
			if c0 < c1 {
				intervals = append(intervals, [2]int{c0, c1})
			}
		}
		intervals = mergeIntervals(intervals)

		out[i] = ChunkLine{
			Num:       num,
			Spans:     spanRunes(runes, intervals),
			Highlight: len(intervals) > 0,
		}
	}
	return out
}

// FileNameSpans splits a filename into matched/unmatched spans using ranges.
// Filename ranges live on line 1; columns are clamped to rune bounds.
func FileNameSpans(name string, ranges []zoekt.Range) []ChunkSpan {
	runes := []rune(name)
	var intervals [][2]int
	for _, rg := range ranges {
		if rg.Start.LineNumber > 1 || rg.End.LineNumber < 1 {
			continue
		}
		c0 := max(0, min(int(rg.Start.Column)-1, len(runes)))
		c1 := max(0, min(int(rg.End.Column)-1, len(runes)))
		if c0 < c1 {
			intervals = append(intervals, [2]int{c0, c1})
		}
	}
	return spanRunes(runes, mergeIntervals(intervals))
}

type CodeSearchParams struct {
	BaseParams
	FilterType  string // "code"
	FilterQuery string
	Results     []SearchResult
	Page        pagination.Page
	HasMore     bool
	ErrorMsg    string

	MatchCount int
	FileCount  int
	TimeTaken  time.Duration
}

func (p *Pages) CodeSearch(w io.Writer, params CodeSearchParams) error {
	params.FilterType = "code"
	return p.execute("search/search", w, params)
}

func (p *Pages) Home(w io.Writer, params TimelineParams) error {
	return p.execute("timeline/home", w, params)
}

type CommentBodyFragmentParams struct {
	Comment     models.Comment
	Reactions   map[models.ReactionKind]models.ReactionDisplayData
	UserReacted map[models.ReactionKind]bool
}

func (p *Pages) CommentBodyFragment(w io.Writer, params CommentBodyFragmentParams) error {
	return p.executePlain("fragments/comment/commentBody", w, params)
}

type PullCommentFragmentParams struct {
	LoggedInUser *oauth.MultiAccountUser
	Comment      models.Comment
	Reactions    map[models.ReactionKind]models.ReactionDisplayData
	UserReacted  map[models.ReactionKind]bool
	HxSwapOob    bool
}

func (p *Pages) PullCommentFragment(w io.Writer, params PullCommentFragmentParams) error {
	return p.executePlain("fragments/comment/pullComment", w, params)
}

type CommentHeaderFragmentParams struct {
	Comment     models.Comment
	Reactions   map[models.ReactionKind]models.ReactionDisplayData
	UserReacted map[models.ReactionKind]bool
	HxSwapOob   bool
}

func (p *Pages) CommentHeaderFragment(w io.Writer, params CommentHeaderFragmentParams) error {
	return p.executePlain("fragments/comment/commentHeader", w, params)
}

type EditCommentFragmentParams struct {
	Comment models.Comment
}

func (p *Pages) EditCommentFragment(w io.Writer, params EditCommentFragmentParams) error {
	return p.executePlain("fragments/comment/edit", w, params)
}

type ReplyCommentFragmentParams struct {
	BaseParams
}

func (p *Pages) ReplyCommentFragment(w io.Writer, params ReplyCommentFragmentParams) error {
	return p.executePlain("fragments/comment/reply", w, params)
}

type ReplyPlaceholderFragmentParams struct {
	BaseParams
}

func (p *Pages) ReplyPlaceholderFragment(w io.Writer, params ReplyPlaceholderFragmentParams) error {
	return p.executePlain("fragments/comment/replyPlaceholder", w, params)
}

func (p *Pages) Static() http.Handler {
	if p.dev {
		return http.StripPrefix("/static/", http.FileServer(http.Dir("appview/pages/static")))
	}

	sub, err := fs.Sub(p.embedFS, "static")
	if err != nil {
		p.logger.Error("no static dir found? that's crazy", "err", err)
		panic(err)
	}
	// Custom handler to apply Cache-Control headers for font files
	return Cache(http.StripPrefix("/static/", http.FileServer(http.FS(sub))))
}

func Cache(h http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		path := strings.Split(r.URL.Path, "?")[0]

		if strings.HasSuffix(path, ".css") {
			// on day for css files
			w.Header().Set("Cache-Control", "public, max-age=86400")
		} else {
			w.Header().Set("Cache-Control", "public, max-age=31536000, immutable")
		}
		h.ServeHTTP(w, r)
	})
}

func (p *Pages) CssContentHash() string {
	cssFile, err := p.embedFS.Open("static/tw.css")
	if err != nil {
		slog.Debug("Error opening CSS file", "err", err)
		return ""
	}
	defer cssFile.Close()

	hasher := sha256.New()
	if _, err := io.Copy(hasher, cssFile); err != nil {
		slog.Debug("Error hashing CSS file", "err", err)
		return ""
	}

	return hex.EncodeToString(hasher.Sum(nil))[:8] // Use first 8 chars of hash
}

func (p *Pages) DangerPasswordTokenStep(w io.Writer) error {
	return p.executePlain("user/settings/fragments/dangerPasswordToken", w, nil)
}

func (p *Pages) DangerPasswordSuccess(w io.Writer) error {
	return p.executePlain("user/settings/fragments/dangerPasswordSuccess", w, nil)
}

func (p *Pages) DangerDeleteTokenStep(w io.Writer) error {
	return p.executePlain("user/settings/fragments/dangerDeleteToken", w, nil)
}

func (p *Pages) Error500(w io.Writer) error {
	return p.execute("errors/500", w, BaseParams{})
}

func (p *Pages) Error404(w io.Writer) error {
	return p.execute("errors/404", w, BaseParams{})
}

func (p *Pages) ErrorKnot404(w io.Writer) error {
	return p.execute("errors/knot404", w, BaseParams{})
}

func (p *Pages) Error503(w io.Writer) error {
	return p.execute("errors/503", w, BaseParams{})
}
