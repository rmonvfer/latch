#import <Foundation/Foundation.h>
#include <errno.h>
#include <limits.h>
#include <signal.h>
#include <stdlib.h>
#include <unistd.h>

// These declarations match Sparkle's public API. Loading the bundled framework
// at runtime also allows command-line tools and unbundled development builds.
@interface SPUUpdater : NSObject
@property(nonatomic, readonly) BOOL canCheckForUpdates;
@property(nonatomic) BOOL automaticallyChecksForUpdates;
- (BOOL)startUpdater:(NSError *__autoreleasing *)error;
@end

@interface SPUStandardUpdaterController : NSObject
@property(nonatomic, readonly) SPUUpdater *updater;
- (instancetype)initWithStartingUpdater:(BOOL)startUpdater
                       updaterDelegate:(id)updaterDelegate
                    userDriverDelegate:(id)userDriverDelegate;
- (void)checkForUpdates:(id)sender;
@end

static SPUStandardUpdaterController *controller;
static NSString *const relaunchContextsKey = @"LatchUpdaterRelaunchContexts";
static const NSTimeInterval relaunchContextLifetime = 600;

static NSString *bundleContextPath(NSBundle *bundle) {
    if (![bundle.bundleURL.pathExtension isEqualToString:@"app"]) return nil;
    return bundle.bundleURL.URLByStandardizingPath.URLByResolvingSymlinksInPath.path;
}

static NSMutableDictionary *relaunchContexts(NSUserDefaults *defaults) {
    id contexts = [defaults objectForKey:relaunchContextsKey];
    return [contexts isKindOfClass:[NSDictionary class]] ? [contexts mutableCopy] : [NSMutableDictionary dictionary];
}

static void saveRelaunchContext(NSBundle *bundle, NSUserDefaults *defaults) {
    NSString *path = bundleContextPath(bundle);
    if (path == nil) return;
    NSMutableDictionary *contexts = relaunchContexts(defaults);
    [contexts removeObjectForKey:path];
    const char *value = getenv("XDG_CONFIG_HOME");
    NSString *configHome = value == NULL ? nil : [[NSString alloc] initWithUTF8String:value];
    NSString *build = [bundle objectForInfoDictionaryKey:@"CFBundleVersion"];
    if (configHome != nil && [build isKindOfClass:[NSString class]]) {
        if (!configHome.isAbsolutePath) {
            configHome = [[NSFileManager defaultManager].currentDirectoryPath stringByAppendingPathComponent:configHome];
        }
        contexts[path] = @{
            @"configHome": configHome.stringByStandardizingPath,
            @"build": build,
            @"timestamp": @([NSDate timeIntervalSinceReferenceDate]),
            @"pid": @(getpid()),
        };
    }
    [defaults setObject:contexts forKey:relaunchContextsKey];
    [defaults synchronize];
}

static bool restoreRelaunchContext(NSBundle *bundle, NSUserDefaults *defaults) {
    NSString *path = bundleContextPath(bundle);
    if (path == nil) return false;
    NSMutableDictionary *contexts = relaunchContexts(defaults);
    id record = contexts[path];
    if (record == nil) return false;
    // Consume the handoff even when a caller supplied an explicit profile or
    // the installation was interrupted, so later launches cannot inherit it.
    [contexts removeObjectForKey:path];
    [defaults setObject:contexts forKey:relaunchContextsKey];
    [defaults synchronize];
    if (getenv("XDG_CONFIG_HOME") != NULL || ![record isKindOfClass:[NSDictionary class]]) return false;
    id configHome = record[@"configHome"];
    id build = record[@"build"];
    id timestamp = record[@"timestamp"];
    id process = record[@"pid"];
    NSString *currentBuild = [bundle objectForInfoDictionaryKey:@"CFBundleVersion"];
    if (![configHome isKindOfClass:[NSString class]] || ![configHome isAbsolutePath] ||
        ![build isKindOfClass:[NSString class]] || ![currentBuild isKindOfClass:[NSString class]] ||
        ![timestamp isKindOfClass:[NSNumber class]] || ![process isKindOfClass:[NSNumber class]] ||
        [build isEqualToString:currentBuild]) return false;
    NSTimeInterval age = [NSDate timeIntervalSinceReferenceDate] - [timestamp doubleValue];
    long long pid = [process longLongValue];
    if (!(age >= 0 && age <= relaunchContextLifetime) || pid <= 0 || pid > INT_MAX) return false;
    // An alive or reused PID fails closed. The installer must have finished
    // terminating the process that recorded this profile before it is used.
    if (kill((pid_t)pid, 0) != -1 || errno != ESRCH) return false;
    return setenv("XDG_CONFIG_HOME", [configHome fileSystemRepresentation], 0) == 0;
}

@interface LatchUpdaterDelegate : NSObject
- (void)updaterWillRelaunchApplication:(SPUUpdater *)updater;
@end

@implementation LatchUpdaterDelegate
- (void)updaterWillRelaunchApplication:(SPUUpdater *)updater {
    (void)updater;
    saveRelaunchContext([NSBundle mainBundle], [NSUserDefaults standardUserDefaults]);
}
@end

static LatchUpdaterDelegate *relaunchDelegate;

bool latch_updater_restore_relaunch_environment(void) {
    if (![NSThread isMainThread]) return false;
    @autoreleasepool {
        return restoreRelaunchContext([NSBundle mainBundle], [NSUserDefaults standardUserDefaults]);
    }
}

int latch_updater_start(void) {
    if (![NSThread isMainThread]) return -1;
    if (controller != nil) return 1;
    NSBundle *app = [NSBundle mainBundle];
    if (![app.bundleURL.pathExtension isEqualToString:@"app"]) return 0;
    NSString *path = [app.privateFrameworksPath stringByAppendingPathComponent:@"Sparkle.framework"];
    NSBundle *framework = [NSBundle bundleWithPath:path];
    NSError *error = nil;
    if (framework == nil || ![framework loadAndReturnError:&error]) {
        NSLog(@"Latch updater framework is unavailable: %@", error);
        return -1;
    }
    Class updaterClass = [framework classNamed:@"SPUStandardUpdaterController"];
    if (updaterClass == Nil) return -1;
    LatchUpdaterDelegate *candidateDelegate = [LatchUpdaterDelegate new];
    SPUStandardUpdaterController *candidate = [[updaterClass alloc]
        initWithStartingUpdater:NO updaterDelegate:candidateDelegate userDriverDelegate:nil];
    if (candidate == nil || ![candidate.updater startUpdater:&error]) {
        NSLog(@"Latch updater could not start: %@", error);
        return -1;
    }
    relaunchDelegate = candidateDelegate;
    controller = candidate;
    return 1;
}

bool latch_updater_can_check(void) {
    return [NSThread isMainThread] && controller != nil && controller.updater.canCheckForUpdates;
}

void latch_updater_check(void) {
    if (latch_updater_can_check()) [controller checkForUpdates:nil];
}

bool latch_updater_automatic_checks(void) {
    return [NSThread isMainThread] && controller != nil && controller.updater.automaticallyChecksForUpdates;
}

void latch_updater_set_automatic_checks(bool enabled) {
    if ([NSThread isMainThread] && controller != nil) {
        controller.updater.automaticallyChecksForUpdates = enabled;
    }
}

int latch_updater_channel(void) {
    NSString *channel = [[NSBundle mainBundle] objectForInfoDictionaryKey:@"LatchUpdateChannel"];
    if ([channel isEqualToString:@"beta"]) return 1;
    if ([channel isEqualToString:@"production"]) return 0;
    return 2;
}
