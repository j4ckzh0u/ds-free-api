import { useEffect, useState, useRef } from 'react';
import { useOutletContext } from 'react-router-dom';
import {
  apiCaptureStart,
  apiCaptureStatus,
  apiFetchConfig,
  apiSaveConfig,
  localizeAuthError,
  type CaptureStatus,
  type FullConfig,
} from '@/lib/api';
import { Card, CardContent, CardHeader, CardTitle, CardDescription } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Badge } from '@/components/ui/badge';
import {
  Key,
  User,
  Tags,
  Boxes,
  Plus,
  Trash2,
  Copy,
  Check,
  Eye,
  EyeOff,
  Save,
  CheckCircle2,
  AlertCircle,
  Sliders,
  Globe,
} from 'lucide-react';
import { Skeleton } from '@/components/ui/skeleton';
import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/utils';

function generateApiKey(): string {
  const bytes = new Uint8Array(24);
  crypto.getRandomValues(bytes);
  return 'sk-' + Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
}

export function ConfigPage() {
  const { t, i18n } = useTranslation();
  const [config, setConfig] = useState<FullConfig | null>(null);
  const [initialConfig, setInitialConfig] = useState<string>('');
  const [saving, setSaving] = useState(false);
  const [message, setMessage] = useState<{ type: 'ok' | 'err'; text: string } | null>(null);
  const [revealedKeys, setRevealedKeys] = useState<Record<number, boolean>>({});
  const [revealedPasswords, setRevealedPasswords] = useState<Record<number, boolean>>({});
  const [revealedTokens, setRevealedTokens] = useState<Record<number, boolean>>({});
  const [copiedKeyIdx, setCopiedKeyIdx] = useState<number | null>(null);
  // 浏览器采集登录
  const [captureRemark, setCaptureRemark] = useState('');
  const [captureStatus, setCaptureStatus] = useState<CaptureStatus | null>(null);
  const captureTimerRef = useRef<ReturnType<typeof setInterval> | null>(null);
  // 轮询回调里判断「有无未保存修改」用的镜像（避免在 setState updater 里做副作用）
  const configSnapshotRef = useRef<{ current: string; initial: string }>({ current: '', initial: '' });

  useEffect(() => {
    apiFetchConfig()
      .then((cfg) => {
        setConfig(cfg);
        setInitialConfig(JSON.stringify(cfg));
      })
      .catch(() => setMessage({ type: 'err', text: t('config.loadFailed') }));
  }, [t]);

  useEffect(() => {
    configSnapshotRef.current = {
      current: config ? JSON.stringify(config) : '',
      initial: initialConfig,
    };
  }, [config, initialConfig]);

  // 启动浏览器采集登录，并每 2 秒轮询状态；成功且无未保存修改时刷新账号列表
  const startCapture = async () => {
    if (captureStatus?.phase === 'running') return;
    try {
      await apiCaptureStart(captureRemark.trim() || undefined);
      setCaptureStatus({ phase: 'running', message: '', seen_api_requests: 0 });
    } catch (e) {
      setCaptureStatus({
        phase: 'failed',
        message: e instanceof Error ? e.message : String(e),
        seen_api_requests: 0,
      });
      return;
    }
    if (captureTimerRef.current) clearInterval(captureTimerRef.current);
    captureTimerRef.current = setInterval(async () => {
      try {
        const st = await apiCaptureStatus();
        setCaptureStatus(st);
        if (st.phase === 'idle' || st.phase === 'running') return;
        clearInterval(captureTimerRef.current!);
        captureTimerRef.current = null;
        if (st.phase === 'success') {
          setCaptureRemark('');
          // 无未保存修改时才自动刷新；有修改则保留本地编辑，由用户手动刷新
          const snap = configSnapshotRef.current;
          if (snap.current && snap.current === snap.initial) {
            apiFetchConfig()
              .then((cfg) => {
                setConfig(cfg);
                setInitialConfig(JSON.stringify(cfg));
              })
              .catch(() => {});
          }
        }
      } catch {
        // 轮询失败忽略，下个周期重试
      }
    }, 2000);
  };

  useEffect(
    () => () => {
      if (captureTimerRef.current) clearInterval(captureTimerRef.current);
    },
    []
  );

  const { isSidebarCollapsed } = useOutletContext<{ isSidebarCollapsed?: boolean }>() || {};
  const [isHeaderActionVisible, setIsHeaderActionVisible] = useState(true);
  const headerActionRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const el = headerActionRef.current;
    if (!el) return;
    const observer = new IntersectionObserver(
      ([entry]) => {
        setIsHeaderActionVisible(entry.isIntersecting);
      },
      { threshold: 0.1 }
    );
    observer.observe(el);
    return () => observer.disconnect();
  }, [config]);

  const isDirty = config ? JSON.stringify(config) !== initialConfig : false;

  if (!config) {
    return (
      <div className="space-y-6 max-w-5xl">
        <div className="flex items-center justify-between pb-3 border-b">
          <div className="space-y-1.5">
            <Skeleton className="h-7 w-48" />
            <Skeleton className="h-4 w-72" />
          </div>
        </div>
        <div className="space-y-5">
          {Array.from({ length: 3 }).map((_, idx) => (
            <Card key={idx} className="p-4 space-y-4 border shadow-sm">
              <div className="flex items-center justify-between">
                <Skeleton className="h-5 w-36" />
                <Skeleton className="h-5 w-16 rounded-full" />
              </div>
              <Skeleton className="h-20 w-full rounded-xl" />
            </Card>
          ))}
        </div>
      </div>
    );
  }

  const update = <T,>(path: string[], value: T) => {
    setConfig((prev) => {
      if (!prev) return prev;
      const next = structuredClone(prev) as unknown as Record<string, unknown>;
      let obj: Record<string, unknown> = next;
      for (let i = 0; i < path.length - 1; i++) {
        obj = obj[path[i]] as Record<string, unknown>;
      }
      obj[path[path.length - 1]] = value as unknown;
      return next as unknown as FullConfig;
    });
  };

  const handleSave = async () => {
    setSaving(true);
    setMessage(null);
    try {
      const body: Record<string, unknown> = {
        server: config.server,
        ds_core: config.ds_core,
        deepseek: config.ds_core,
        accounts: config.ds_core.accounts,
        proxy: config.proxy,
        admin: {
          password_hash: '',
          jwt_secret: '',
          jwt_issued_at: config.admin.jwt_issued_at,
          old_password: '',
          new_password: '',
        },
        api_keys: config.api_keys.map((k) => ({
          key: k.key,
          description: k.description,
        })),
      };
      const res = await apiSaveConfig(body);
      if (res.ok) {
        setMessage({ type: 'ok', text: t('config.saveSuccess') });
        setRevealedKeys({});
        const fresh = await apiFetchConfig();
        setConfig(fresh);
        setInitialConfig(JSON.stringify(fresh));
      }
    } catch (e: unknown) {
      const rawMsg = e instanceof Error ? e.message : String(e);
      setMessage({ type: 'err', text: localizeAuthError(rawMsg, i18n.language) || t('config.saveFailed') });
    } finally {
      setSaving(false);
    }
  };

  const handleCancel = () => {
    if (confirm(t('config.cancelConfirm'))) {
      setRevealedKeys({});
      apiFetchConfig()
        .then((cfg) => {
          setConfig(cfg);
          setInitialConfig(JSON.stringify(cfg));
        })
        .catch(() => setMessage({ type: 'err', text: t('config.loadFailed') }));
    }
  };

  const copyToClipboard = (text: string, idx: number) => {
    navigator.clipboard.writeText(text);
    setCopiedKeyIdx(idx);
    setTimeout(() => setCopiedKeyIdx(null), 2000);
  };

  return (
    <div className="space-y-6 max-w-5xl pb-24 sm:pb-28 relative">
      {/* Page Header */}
      <div className="flex items-center justify-between gap-3 pb-3 border-b min-w-0">
        <div className="min-w-0 flex-1">
          <h1 className="text-xl sm:text-2xl font-bold tracking-tight flex items-center gap-2.5 truncate">
            <Sliders className="h-5 w-5 sm:h-6 sm:w-6 text-primary shrink-0" />
            <span className="truncate">{t('config.title')}</span>
          </h1>
          <p className="text-xs sm:text-sm text-muted-foreground mt-0.5 truncate">
            {t('config.subtitle')}
          </p>
        </div>

        <div ref={headerActionRef} className="flex items-center gap-2 shrink-0">
          <Button
            variant="outline"
            size="sm"
            onClick={handleCancel}
            disabled={saving || !isDirty}
            className="h-8 text-xs shrink-0"
          >
            {t('config.cancel')}
          </Button>
          <Button
            size="sm"
            onClick={handleSave}
            disabled={saving}
            className="h-8 gap-1.5 text-xs shadow-sm shrink-0"
          >
            <Save className="h-3.5 w-3.5" />
            <span>{saving ? t('config.saving') : t('config.save')}</span>
          </Button>
        </div>
      </div>

      {/* Alert Message */}
      {message && (
        <div
          className={`flex items-center gap-2.5 p-3 rounded-xl text-xs font-medium border ${
            message.type === 'err'
              ? 'bg-destructive/10 text-destructive border-destructive/20'
              : 'bg-emerald-500/10 text-emerald-600 dark:text-emerald-400 border-emerald-500/20'
          }`}
        >
          {message.type === 'err' ? (
            <AlertCircle className="h-4 w-4 shrink-0" />
          ) : (
            <CheckCircle2 className="h-4 w-4 shrink-0" />
          )}
          <span>{message.text}</span>
        </div>
      )}

      {/* ── 1. DeepSeek Web Accounts Pool ─────────────────────────── */}
      <Card className="border shadow-sm">
        <CardHeader className="pb-3">
          <div className="flex items-center justify-between">
            <CardTitle className="text-base font-semibold flex items-center gap-2">
              <User className="h-4 w-4 text-primary" />
              <span>{t('config.sections.accounts')}</span>
            </CardTitle>
            <Badge variant="secondary" className="font-mono text-xs">
              {t('config.accounts.countLabel', { count: config.ds_core.accounts.length })}
            </Badge>
          </div>
          <CardDescription className="text-xs">
            {t('config.accounts.description')}
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-3 pt-2">
          {config.ds_core.accounts.length === 0 ? (
            <div className="text-center py-6 border border-dashed rounded-xl text-xs text-muted-foreground">
              {t('config.accounts.empty')}
            </div>
          ) : (
            <div className="divide-y border rounded-xl overflow-hidden bg-card/40">
              {config.ds_core.accounts.map((a, i) => (
                <div
                  key={i}
                  className="p-3.5 sm:p-3 space-y-3 sm:space-y-0 hover:bg-muted/15 transition-colors"
                >
                  {/* Mobile Card Header */}
                  <div className="flex sm:hidden items-center justify-between pb-2 border-b">
                    <span className="text-xs font-semibold font-mono text-primary">
                      {t('config.accounts.accountNumber', { num: i + 1 })}
                    </span>
                    <Button
                      variant="ghost"
                      size="sm"
                      onClick={() =>
                        update(
                          ['ds_core', 'accounts'],
                          config.ds_core.accounts.filter((_, j) => j !== i)
                        )
                      }
                      className="h-7 px-2 text-xs text-destructive hover:bg-destructive/10"
                    >
                      <Trash2 className="h-3.5 w-3.5 mr-1" />
                      {t('config.accounts.deleteLabel')}
                    </Button>
                  </div>

                  {/* Fields (Stacked on mobile, 12-col grid on desktop/tablet) */}
                  <div className="grid grid-cols-1 sm:grid-cols-12 gap-2.5 items-end">
                    <div className="sm:col-span-3">
                      <label htmlFor={`acc-email-${i}`} className="text-[11px] font-medium text-muted-foreground block mb-1">
                        {t('config.accounts.email')}
                      </label>
                      <Input
                        id={`acc-email-${i}`}
                        placeholder="user@example.com"
                        value={a.email}
                        onChange={(e) => {
                          const next = [...config.ds_core.accounts];
                          next[i] = { ...next[i], email: e.target.value };
                          update(['ds_core', 'accounts'], next);
                        }}
                        className="text-xs font-mono"
                      />
                    </div>

                    <div className="sm:col-span-2">
                      <label htmlFor={`acc-mobile-${i}`} className="text-[11px] font-medium text-muted-foreground block mb-1">
                        {t('config.accounts.mobile')}
                      </label>
                      <Input
                        id={`acc-mobile-${i}`}
                        placeholder="13800138000"
                        value={a.mobile}
                        onChange={(e) => {
                          const next = [...config.ds_core.accounts];
                          next[i] = { ...next[i], mobile: e.target.value };
                          update(['ds_core', 'accounts'], next);
                        }}
                        className="text-xs font-mono"
                      />
                    </div>

                    <div className="sm:col-span-1">
                      <label htmlFor={`acc-code-${i}`} className="text-[11px] font-medium text-muted-foreground block mb-1">
                        {t('config.accounts.areaCode')}
                      </label>
                      <Input
                        id={`acc-code-${i}`}
                        placeholder="+86"
                        value={a.area_code}
                        onChange={(e) => {
                          const next = [...config.ds_core.accounts];
                          next[i] = { ...next[i], area_code: e.target.value };
                          update(['ds_core', 'accounts'], next);
                        }}
                        className="text-xs font-mono"
                      />
                    </div>

                    <div className="sm:col-span-2">
                      <label htmlFor={`acc-pass-${i}`} className="text-[11px] font-medium text-muted-foreground block mb-1">
                        {t('config.accounts.password')}
                      </label>
                      <div className="relative">
                        <Input
                          id={`acc-pass-${i}`}
                          type={revealedPasswords[i] ? 'text' : 'password'}
                          placeholder="••••••••"
                          value={a.password}
                          onChange={(e) => {
                            const next = [...config.ds_core.accounts];
                            next[i] = { ...next[i], password: e.target.value };
                            update(['ds_core', 'accounts'], next);
                          }}
                          className="text-xs pr-8"
                        />
                        <button
                          type="button"
                          onClick={() => setRevealedPasswords((prev) => ({ ...prev, [i]: !prev[i] }))}
                          className="absolute right-2 top-1/2 -translate-y-1/2 text-muted-foreground hover:text-foreground"
                          aria-label="Toggle password visibility"
                        >
                          {revealedPasswords[i] ? <EyeOff className="h-3.5 w-3.5" /> : <Eye className="h-3.5 w-3.5" />}
                        </button>
                      </div>
                    </div>

                    <div className="sm:col-span-3">
                      <label htmlFor={`acc-device-id-${i}`} className="text-[11px] font-medium text-muted-foreground block mb-1">
                        {t('config.accounts.deviceId')}
                      </label>
                      <Input
                        id={`acc-device-id-${i}`}
                        placeholder={t('config.accounts.deviceIdPlaceholder')}
                        value={a.device_id ?? ''}
                        onChange={(e) => {
                          const next = [...config.ds_core.accounts];
                          next[i] = { ...next[i], device_id: e.target.value };
                          update(['ds_core', 'accounts'], next);
                        }}
                        className="text-xs font-mono"
                      />
                    </div>

                    {/* 手动令牌模式：非空时跳过登录（换行到第二行网格） */}
                    <div className="sm:col-span-3">
                      <label htmlFor={`acc-token-${i}`} className="text-[11px] font-medium text-muted-foreground block mb-1">
                        {t('config.accounts.token')}
                      </label>
                      <div className="relative">
                        <Input
                          id={`acc-token-${i}`}
                          type={revealedTokens[i] ? 'text' : 'password'}
                          placeholder={t('config.accounts.tokenPlaceholder')}
                          value={a.token ?? ''}
                          onChange={(e) => {
                            const next = [...config.ds_core.accounts];
                            next[i] = { ...next[i], token: e.target.value };
                            update(['ds_core', 'accounts'], next);
                          }}
                          className="text-xs font-mono pr-8"
                        />
                        <button
                          type="button"
                          onClick={() => setRevealedTokens((prev) => ({ ...prev, [i]: !prev[i] }))}
                          className="absolute right-2 top-1/2 -translate-y-1/2 text-muted-foreground hover:text-foreground"
                          aria-label="Toggle token visibility"
                        >
                          {revealedTokens[i] ? <EyeOff className="h-3.5 w-3.5" /> : <Eye className="h-3.5 w-3.5" />}
                        </button>
                      </div>
                    </div>

                    <div className="sm:col-span-3">
                      <label htmlFor={`acc-cookies-${i}`} className="text-[11px] font-medium text-muted-foreground block mb-1">
                        {t('config.accounts.cookies')}
                      </label>
                      <Input
                        id={`acc-cookies-${i}`}
                        placeholder={t('config.accounts.cookiesPlaceholder')}
                        value={a.cookies ?? ''}
                        onChange={(e) => {
                          const next = [...config.ds_core.accounts];
                          next[i] = { ...next[i], cookies: e.target.value };
                          update(['ds_core', 'accounts'], next);
                        }}
                        className="text-xs font-mono"
                      />
                    </div>

                    <div className="sm:col-span-3">
                      <label htmlFor={`acc-remark-${i}`} className="text-[11px] font-medium text-muted-foreground block mb-1">
                        {t('config.accounts.remark')}
                      </label>
                      <Input
                        id={`acc-remark-${i}`}
                        placeholder={t('config.accounts.remarkPlaceholder')}
                        value={a.remark ?? ''}
                        onChange={(e) => {
                          const next = [...config.ds_core.accounts];
                          next[i] = { ...next[i], remark: e.target.value };
                          update(['ds_core', 'accounts'], next);
                        }}
                        className="text-xs"
                      />
                    </div>

                    {/* Desktop Delete Button */}
                    <div className="hidden sm:flex sm:col-span-1 justify-end">
                      <Button
                        variant="ghost"
                        size="icon"
                        onClick={() =>
                          update(
                            ['ds_core', 'accounts'],
                            config.ds_core.accounts.filter((_, j) => j !== i)
                          )
                        }
                        className="h-8 w-8 text-destructive hover:bg-destructive/10"
                        title={t('config.accounts.deleteTitle')}
                      >
                        <Trash2 className="h-4 w-4" />
                      </Button>
                    </div>
                  </div>
                </div>
              ))}
            </div>
          )}

          {/* 浏览器采集登录：打开真实 Chrome 人工登录，自动采集 token/cookies */}
          <div className="flex flex-col sm:flex-row gap-2 sm:items-center pt-1">
            <Button
              variant="outline"
              size="sm"
              onClick={startCapture}
              disabled={captureStatus?.phase === 'running'}
              className="gap-1.5 text-xs shrink-0"
            >
              <Globe className="h-3.5 w-3.5" />
              <span>{t('config.capture.button')}</span>
            </Button>
            <Input
              placeholder={t('config.capture.remarkPlaceholder')}
              value={captureRemark}
              onChange={(e) => setCaptureRemark(e.target.value)}
              disabled={captureStatus?.phase === 'running'}
              className="h-8 text-xs flex-1"
            />
          </div>
          {captureStatus && captureStatus.phase !== 'idle' && (
            <div
              className={cn(
                'flex items-start gap-1.5 text-xs rounded-lg px-2.5 py-2 border',
                captureStatus.phase === 'running' && 'text-muted-foreground bg-muted/20',
                captureStatus.phase === 'success' && 'text-green-600 bg-green-500/10 border-green-500/30',
                captureStatus.phase === 'failed' && 'text-destructive bg-destructive/10 border-destructive/30'
              )}
            >
              {captureStatus.phase === 'success' ? (
                <CheckCircle2 className="h-3.5 w-3.5 mt-0.5 shrink-0" />
              ) : captureStatus.phase === 'failed' ? (
                <AlertCircle className="h-3.5 w-3.5 mt-0.5 shrink-0" />
              ) : (
                <Globe className="h-3.5 w-3.5 mt-0.5 shrink-0 animate-pulse" />
              )}
              <span className="min-w-0 break-words">
                {captureStatus.phase === 'running' &&
                  `${t('config.capture.running')} (${captureStatus.seen_api_requests})`}
                {captureStatus.phase === 'success' &&
                  t('config.capture.success', { label: captureStatus.account_label ?? '' })}
                {captureStatus.phase === 'failed' &&
                  t('config.capture.failed', { message: captureStatus.message })}
              </span>
            </div>
          )}

          <Button
            variant="outline"
            size="sm"
            onClick={() =>
              update(['ds_core', 'accounts'], [
                ...config.ds_core.accounts,
                { email: '', mobile: '', area_code: '', password: '', device_id: '', token: '', cookies: '' },
              ])
            }
            className="gap-1.5 text-xs mt-2"
          >
            <Plus className="h-3.5 w-3.5" />
            <span>{t('config.accounts.add')}</span>
          </Button>
        </CardContent>
      </Card>

      {/* ── 2. Client API Keys Management ─────────────────────────── */}
      <Card className="border shadow-sm">
        <CardHeader className="pb-3">
          <div className="flex items-center justify-between">
            <CardTitle className="text-base font-semibold flex items-center gap-2">
              <Key className="h-4 w-4 text-primary" />
              <span>{t('config.sections.apiKeys')}</span>
            </CardTitle>
            <Badge variant="secondary" className="font-mono text-xs">
              {t('config.apiKeys.countLabel', { count: config.api_keys.length })}
            </Badge>
          </div>
          <CardDescription className="text-xs">
            {t('config.apiKeys.cardDescription')}
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-3 pt-2">
          {config.api_keys.length === 0 ? (
            <div className="text-center py-6 border border-dashed rounded-xl text-xs text-muted-foreground">
              {t('config.apiKeys.empty')}
            </div>
          ) : (
            <div className="divide-y border rounded-xl overflow-hidden bg-card/40">
              {config.api_keys.map((k, i) => (
                <div
                  key={i}
                  className="p-3.5 sm:p-2.5 space-y-2.5 sm:space-y-0 sm:flex sm:items-center sm:gap-2 hover:bg-muted/15 transition-colors"
                >
                  {/* Mobile Card Header */}
                  <div className="flex sm:hidden items-center justify-between pb-1.5 border-b">
                    <span className="text-xs font-semibold font-mono text-primary">
                      API Key #{i + 1}
                    </span>
                    <Button
                      variant="ghost"
                      size="sm"
                      onClick={() => update(['api_keys'], config.api_keys.filter((_, j) => j !== i))}
                      className="h-7 px-2 text-xs text-destructive hover:bg-destructive/10"
                    >
                      <Trash2 className="h-3.5 w-3.5 mr-1" />
                      {t('config.accounts.deleteLabel')}
                    </Button>
                  </div>

                  <div className="flex-1 flex items-center gap-1.5 min-w-0">
                    <Input
                      type={revealedKeys[i] ? 'text' : 'password'}
                      value={k.key}
                      onChange={(e) => {
                        const next = [...config.api_keys];
                        next[i] = { ...next[i], key: e.target.value };
                        update(['api_keys'], next);
                      }}
                      className="font-mono text-xs flex-1"
                    />
                    <Button
                      variant="ghost"
                      size="icon"
                      onClick={() => setRevealedKeys((prev) => ({ ...prev, [i]: !prev[i] }))}
                      className="h-8 w-8 shrink-0 text-muted-foreground hover:text-foreground"
                      title={t('config.apiKeys.toggleVisibility')}
                    >
                      {revealedKeys[i] ? <EyeOff className="h-3.5 w-3.5" /> : <Eye className="h-3.5 w-3.5" />}
                    </Button>
                    <Button
                      variant="ghost"
                      size="icon"
                      onClick={() => copyToClipboard(k.key, i)}
                      className="h-8 w-8 shrink-0 text-muted-foreground hover:text-foreground"
                      title={t('config.apiKeys.copyKeyTitle')}
                    >
                      {copiedKeyIdx === i ? (
                        <Check className="h-3.5 w-3.5 text-emerald-500" />
                      ) : (
                        <Copy className="h-3.5 w-3.5" />
                      )}
                    </Button>
                  </div>

                  <div className="flex items-center gap-1.5 sm:w-64">
                    <Input
                      placeholder={t('config.apiKeys.placeholder')}
                      value={k.description}
                      onChange={(e) => {
                        const next = [...config.api_keys];
                        next[i] = { ...next[i], description: e.target.value };
                        update(['api_keys'], next);
                      }}
                      className="text-xs flex-1"
                    />
                    <Button
                      variant="ghost"
                      size="icon"
                      onClick={() => update(['api_keys'], config.api_keys.filter((_, j) => j !== i))}
                      className="hidden sm:flex h-8 w-8 shrink-0 text-destructive hover:bg-destructive/10"
                      title={t('config.apiKeys.deleteKeyTitle')}
                    >
                      <Trash2 className="h-4 w-4" />
                    </Button>
                  </div>
                </div>
              ))}
            </div>
          )}

          <Button
            variant="outline"
            size="sm"
            onClick={() =>
              update(['api_keys'], [
                ...config.api_keys,
                { key: generateApiKey(), description: 'Default API Key' },
              ])
            }
            className="gap-1.5 text-xs mt-2"
          >
            <Plus className="h-3.5 w-3.5" />
            <span>{t('config.apiKeys.add')}</span>
          </Button>
        </CardContent>
      </Card>

      {/* ── 3. Model Types & Token Limits ─────────────────────────── */}
      <Card className="border shadow-sm">
        <CardHeader className="pb-3">
          <CardTitle className="text-base font-semibold flex items-center gap-2">
            <Boxes className="h-4 w-4 text-primary" />
            <span>{t('config.sections.models')}</span>
          </CardTitle>
          <CardDescription className="text-xs">
            {t('config.modelsSection.description')}
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-3 pt-2">
          <div className="divide-y border rounded-xl overflow-hidden bg-card/40">
            {config.ds_core.model_types.map((type, i) => (
              <div
                key={i}
                className="grid grid-cols-1 sm:grid-cols-12 gap-2.5 p-3 items-end hover:bg-muted/15 transition-colors"
              >
                <div className="sm:col-span-2">
                  <label htmlFor={`mod-type-${i}`} className="text-[11px] font-medium text-muted-foreground block mb-1">
                    {t('config.modelsSection.typeName')}
                  </label>
                  <Input
                    id={`mod-type-${i}`}
                    value={type}
                    onChange={(e) => {
                      const next = [...config.ds_core.model_types];
                      next[i] = e.target.value;
                      update(['ds_core', 'model_types'], next);
                    }}
                    className="font-mono text-xs"
                  />
                </div>

                <div className="sm:col-span-2">
                  <label htmlFor={`mod-in-${i}`} className="text-[11px] font-medium text-muted-foreground block mb-1">
                    {t('config.modelsSection.maxInput')}
                  </label>
                  <Input
                    id={`mod-in-${i}`}
                    type="number"
                    value={config.ds_core.max_input_tokens[i] ?? 1048576}
                    onChange={(e) => {
                      const next = [...config.ds_core.max_input_tokens];
                      next[i] = Number(e.target.value);
                      update(['ds_core', 'max_input_tokens'], next);
                    }}
                    className="font-mono text-xs"
                  />
                </div>

                <div className="sm:col-span-2">
                  <label htmlFor={`mod-out-${i}`} className="text-[11px] font-medium text-muted-foreground block mb-1">
                    {t('config.modelsSection.maxOutput')}
                  </label>
                  <Input
                    id={`mod-out-${i}`}
                    type="number"
                    value={config.ds_core.max_output_tokens[i] ?? 384000}
                    onChange={(e) => {
                      const next = [...config.ds_core.max_output_tokens];
                      next[i] = Number(e.target.value);
                      update(['ds_core', 'max_output_tokens'], next);
                    }}
                    className="font-mono text-xs"
                  />
                </div>

                <div className="sm:col-span-3">
                  <label htmlFor={`mod-chars-${i}`} className="text-[11px] font-medium text-muted-foreground block mb-1">
                    {t('config.modelsSection.inputCharLimit')}
                  </label>
                  <Input
                    id={`mod-chars-${i}`}
                    type="number"
                    value={config.ds_core.input_character_limits[i] ?? 2621440}
                    onChange={(e) => {
                      const next = [...config.ds_core.input_character_limits];
                      next[i] = Number(e.target.value);
                      update(['ds_core', 'input_character_limits'], next);
                    }}
                    className="font-mono text-xs"
                  />
                </div>

                <div className="sm:col-span-2">
                  <label htmlFor={`mod-alias-${i}`} className="text-[11px] font-medium text-muted-foreground block mb-1">
                    {t('config.modelsSection.alias')}
                  </label>
                  <Input
                    id={`mod-alias-${i}`}
                    placeholder="deepseek-chat"
                    value={config.ds_core.model_aliases?.[i] || ''}
                    onChange={(e) => {
                      const next = [...(config.ds_core.model_aliases || [])];
                      next[i] = e.target.value;
                      update(['ds_core', 'model_aliases'], next);
                    }}
                    className="font-mono text-xs"
                  />
                </div>

                <div className="sm:col-span-1 flex justify-end">
                  <Button
                    variant="ghost"
                    size="icon"
                    onClick={() => {
                      // 所有按 index 对齐 model_types 的数组必须同步增删，
                      // 否则后端 validate() 会以长度不一致拒绝保存。
                      const drop = <T,>(arr: T[]) => arr.filter((_, j) => j !== i);
                      const core = config.ds_core;
                      update(['ds_core', 'model_types'], drop(core.model_types));
                      update(['ds_core', 'max_input_tokens'], drop(core.max_input_tokens));
                      update(['ds_core', 'max_output_tokens'], drop(core.max_output_tokens));
                      update(['ds_core', 'input_character_limits'], drop(core.input_character_limits));
                      update(['ds_core', 'model_aliases'], drop(core.model_aliases ?? []));
                    }}
                    className="h-8 w-8 text-destructive hover:bg-destructive/10"
                    title={t('config.modelsSection.deleteModelTitle')}
                  >
                    <Trash2 className="h-4 w-4" />
                  </Button>
                </div>
              </div>
            ))}
          </div>

          <Button
            variant="outline"
            size="sm"
            onClick={() => {
              // 新条目的默认值须与 src/config.rs 的 default_* 保持一致
              const core = config.ds_core;
              update(['ds_core', 'model_types'], [...core.model_types, 'custom']);
              update(['ds_core', 'max_input_tokens'], [...core.max_input_tokens, 1048576]);
              update(['ds_core', 'max_output_tokens'], [...core.max_output_tokens, 384000]);
              update(
                ['ds_core', 'input_character_limits'],
                [...core.input_character_limits, 2621440]
              );
              update(['ds_core', 'model_aliases'], [...(core.model_aliases ?? []), '']);
            }}
            className="gap-1.5 text-xs mt-2"
          >
            <Plus className="h-3.5 w-3.5" />
            <span>{t('config.modelsSection.add')}</span>
          </Button>
        </CardContent>
      </Card>

      {/* ── 4. Tool Call Tags ─────────────────────────────────────── */}
      <Card className="border shadow-sm">
        <CardHeader className="pb-3">
          <CardTitle className="text-base font-semibold flex items-center gap-2">
            <Tags className="h-4 w-4 text-primary" />
            <span>{t('config.sections.toolCallTags')}</span>
          </CardTitle>
          <CardDescription className="text-xs">
            {t('config.toolCallTags.description')}
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4 pt-2">
          <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
            <div>
              <label htmlFor="tag-starts" className="text-xs font-medium text-muted-foreground block mb-1.5">
                {t('config.toolCallTags.extraStarts')}
              </label>
              <div className="flex flex-wrap gap-1.5 p-2 rounded-xl border bg-muted/20 min-h-[42px] items-center">
                {config.ds_core.tool_call.extra_starts.map((tag, i) => (
                  <Badge key={i} variant="secondary" className="font-mono text-xs gap-1 py-0.5">
                    {tag}
                    <button
                      type="button"
                      onClick={() =>
                        update(
                          ['ds_core', 'tool_call', 'extra_starts'],
                          config.ds_core.tool_call.extra_starts.filter((_, j) => j !== i)
                        )
                      }
                      className="ml-1 text-muted-foreground hover:text-foreground"
                    >
                      ×
                    </button>
                  </Badge>
                ))}
              </div>
              <Input
                id="tag-starts"
                placeholder={t('config.toolCallTags.placeholder')}
                className="text-xs font-mono mt-1.5"
                onKeyDown={(e) => {
                  if (e.key === 'Enter' && e.currentTarget.value.trim()) {
                    e.preventDefault();
                    update(['ds_core', 'tool_call', 'extra_starts'], [
                      ...config.ds_core.tool_call.extra_starts,
                      e.currentTarget.value.trim(),
                    ]);
                    e.currentTarget.value = '';
                  }
                }}
              />
            </div>

            <div>
              <label htmlFor="tag-ends" className="text-xs font-medium text-muted-foreground block mb-1.5">
                {t('config.toolCallTags.extraEnds')}
              </label>
              <div className="flex flex-wrap gap-1.5 p-2 rounded-xl border bg-muted/20 min-h-[42px] items-center">
                {config.ds_core.tool_call.extra_ends.map((tag, i) => (
                  <Badge key={i} variant="secondary" className="font-mono text-xs gap-1 py-0.5">
                    {tag}
                    <button
                      type="button"
                      onClick={() =>
                        update(
                          ['ds_core', 'tool_call', 'extra_ends'],
                          config.ds_core.tool_call.extra_ends.filter((_, j) => j !== i)
                        )
                      }
                      className="ml-1 text-muted-foreground hover:text-foreground"
                    >
                      ×
                    </button>
                  </Badge>
                ))}
              </div>
              <Input
                id="tag-ends"
                placeholder={t('config.toolCallTags.placeholder')}
                className="text-xs font-mono mt-1.5"
                onKeyDown={(e) => {
                  if (e.key === 'Enter' && e.currentTarget.value.trim()) {
                    e.preventDefault();
                    update(['ds_core', 'tool_call', 'extra_ends'], [
                      ...config.ds_core.tool_call.extra_ends,
                      e.currentTarget.value.trim(),
                    ]);
                    e.currentTarget.value = '';
                  }
                }}
              />
            </div>
          </div>
        </CardContent>
      </Card>

      {/* ── Floating Action Bar (Docked to Bottom of Viewport, Appears Only When Header Actions Are Scrolled Out of View) ── */}
      <div
        className={cn(
          'fixed bottom-20 md:bottom-4 left-3.5 right-3.5 md:left-20 md:right-6 z-40 max-w-5xl mx-auto flex items-center justify-between p-3 sm:p-3.5 rounded-2xl bg-card/95 backdrop-blur-md border shadow-xl ring-1 ring-black/5 dark:ring-white/10 transition-all duration-300',
          isSidebarCollapsed ? 'lg:left-20 lg:right-6' : 'lg:left-72 lg:right-8',
          isHeaderActionVisible
            ? 'opacity-0 translate-y-6 pointer-events-none'
            : 'opacity-100 translate-y-0 pointer-events-auto'
        )}
      >
        <div className="flex items-center gap-2.5 min-w-0">
          <span className="flex h-2.5 w-2.5 relative shrink-0">
            {isDirty ? (
              <>
                <span className="animate-ping absolute inline-flex h-full w-full rounded-full bg-amber-400 opacity-75" />
                <span className="relative inline-flex rounded-full h-2.5 w-2.5 bg-amber-500" />
              </>
            ) : (
              <span className="relative inline-flex rounded-full h-2.5 w-2.5 bg-green-500" />
            )}
          </span>
          <span className="text-xs font-medium text-foreground truncate">
            {isDirty
              ? t('config.unsavedChanges')
              : t('config.allSynced')}
          </span>
        </div>

        <div className="flex items-center gap-2 shrink-0">
          <Button
            variant="outline"
            size="sm"
            onClick={handleCancel}
            disabled={saving || !isDirty}
            className="h-8 text-xs shrink-0"
          >
            {t('config.cancel')}
          </Button>
          <Button
            size="sm"
            onClick={handleSave}
            disabled={saving || !isDirty}
            className="h-8 gap-1.5 text-xs shadow-sm shrink-0"
          >
            <Save className="h-3.5 w-3.5" />
            <span>{saving ? t('config.saving') : t('config.save')}</span>
          </Button>
        </div>
      </div>
    </div>
  );
}
