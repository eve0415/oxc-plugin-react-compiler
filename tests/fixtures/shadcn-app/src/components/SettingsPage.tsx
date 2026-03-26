import { useState, useCallback } from 'react';

import { useLocalStorage } from '../hooks/useLocalStorage';

import { Accordion, AccordionItem, AccordionTrigger, AccordionContent } from './ui/Accordion';
import { Button } from './ui/Button';
import { Card, CardHeader, CardTitle, CardDescription, CardContent } from './ui/Card';
import { Dialog, DialogTrigger, DialogContent, DialogHeader, DialogTitle, DialogFooter } from './ui/Dialog';
import { Input } from './ui/Input';
import { Select, SelectTrigger, SelectContent, SelectItem } from './ui/Select';
import { Switch } from './ui/Switch';
import { Tabs, TabsList, TabsTrigger, TabsContent } from './ui/Tabs';
import { useToast } from './ui/Toast';

interface Settings {
  theme: string;
  language: string;
  fontSize: string;
  reduceMotion: boolean;
  highContrast: boolean;
  autoSave: boolean;
  autoSaveInterval: string;
  compactMode: boolean;
  showAvatars: boolean;
  emailDigest: string;
  twoFactor: boolean;
  sessionTimeout: string;
}

const defaultSettings: Settings = {
  theme: 'system',
  language: 'en',
  fontSize: 'medium',
  reduceMotion: false,
  highContrast: false,
  autoSave: true,
  autoSaveInterval: '30',
  compactMode: false,
  showAvatars: true,
  emailDigest: 'daily',
  twoFactor: false,
  sessionTimeout: '30',
};

export function SettingsPage() {
  const [settings, setSettings, resetSettings] = useLocalStorage<Settings>('app-settings', defaultSettings);
  const [deleteDialogOpen, setDeleteDialogOpen] = useState(false);
  const { addToast } = useToast();

  const updateSetting = useCallback(
    <K extends keyof Settings>(key: K, value: Settings[K]) => {
      setSettings(prev => ({ ...prev, [key]: value }));
    },
    [setSettings],
  );

  const handleSave = useCallback(() => {
    addToast({ title: 'Settings saved', description: 'Your preferences have been updated.' });
  }, [addToast]);

  const handleReset = useCallback(() => {
    resetSettings();
    addToast({ title: 'Settings reset', description: 'All preferences restored to defaults.' });
  }, [resetSettings, addToast]);

  const handleDeleteAccount = useCallback(() => {
    setDeleteDialogOpen(false);
    addToast({ title: 'Account deleted', variant: 'destructive' });
  }, [addToast]);

  return (
    <div className='max-w-4xl mx-auto space-y-6'>
      <div>
        <h1 className='text-3xl font-bold'>Settings</h1>
        <p className='text-muted-foreground'>Manage your account settings and preferences.</p>
      </div>

      <Tabs defaultValue='appearance'>
        <TabsList>
          <TabsTrigger value='appearance'>Appearance</TabsTrigger>
          <TabsTrigger value='editor'>Editor</TabsTrigger>
          <TabsTrigger value='notifications'>Notifications</TabsTrigger>
          <TabsTrigger value='security'>Security</TabsTrigger>
          <TabsTrigger value='danger'>Danger Zone</TabsTrigger>
        </TabsList>

        <TabsContent value='appearance'>
          <Card>
            <CardHeader>
              <CardTitle>Appearance</CardTitle>
              <CardDescription>Customize how the app looks and feels.</CardDescription>
            </CardHeader>
            <CardContent className='space-y-6'>
              <div className='grid grid-cols-2 gap-4'>
                <div>
                  <label className='text-sm font-medium'>Theme</label>
                  <Select value={settings.theme} onValueChange={v => updateSetting('theme', v)}>
                    <SelectTrigger>{settings.theme}</SelectTrigger>
                    <SelectContent>
                      <SelectItem value='light'>Light</SelectItem>
                      <SelectItem value='dark'>Dark</SelectItem>
                      <SelectItem value='system'>System</SelectItem>
                    </SelectContent>
                  </Select>
                </div>
                <div>
                  <label className='text-sm font-medium'>Language</label>
                  <Select value={settings.language} onValueChange={v => updateSetting('language', v)}>
                    <SelectTrigger>{settings.language}</SelectTrigger>
                    <SelectContent>
                      <SelectItem value='en'>English</SelectItem>
                      <SelectItem value='es'>Spanish</SelectItem>
                      <SelectItem value='fr'>French</SelectItem>
                      <SelectItem value='de'>German</SelectItem>
                      <SelectItem value='ja'>Japanese</SelectItem>
                    </SelectContent>
                  </Select>
                </div>
              </div>
              <div>
                <label className='text-sm font-medium'>Font Size</label>
                <Select value={settings.fontSize} onValueChange={v => updateSetting('fontSize', v)}>
                  <SelectTrigger>{settings.fontSize}</SelectTrigger>
                  <SelectContent>
                    <SelectItem value='small'>Small</SelectItem>
                    <SelectItem value='medium'>Medium</SelectItem>
                    <SelectItem value='large'>Large</SelectItem>
                  </SelectContent>
                </Select>
              </div>
              <div className='space-y-4'>
                <div className='flex items-center justify-between'>
                  <div>
                    <p className='text-sm font-medium'>Reduce Motion</p>
                    <p className='text-xs text-muted-foreground'>Minimize animations and transitions.</p>
                  </div>
                  <Switch checked={settings.reduceMotion} onCheckedChange={v => updateSetting('reduceMotion', v)} />
                </div>
                <div className='flex items-center justify-between'>
                  <div>
                    <p className='text-sm font-medium'>High Contrast</p>
                    <p className='text-xs text-muted-foreground'>Increase contrast for better visibility.</p>
                  </div>
                  <Switch checked={settings.highContrast} onCheckedChange={v => updateSetting('highContrast', v)} />
                </div>
                <div className='flex items-center justify-between'>
                  <div>
                    <p className='text-sm font-medium'>Compact Mode</p>
                    <p className='text-xs text-muted-foreground'>Reduce spacing between elements.</p>
                  </div>
                  <Switch checked={settings.compactMode} onCheckedChange={v => updateSetting('compactMode', v)} />
                </div>
                <div className='flex items-center justify-between'>
                  <div>
                    <p className='text-sm font-medium'>Show Avatars</p>
                    <p className='text-xs text-muted-foreground'>Display user avatars in lists.</p>
                  </div>
                  <Switch checked={settings.showAvatars} onCheckedChange={v => updateSetting('showAvatars', v)} />
                </div>
              </div>
            </CardContent>
          </Card>
        </TabsContent>

        <TabsContent value='editor'>
          <Card>
            <CardHeader>
              <CardTitle>Editor Settings</CardTitle>
              <CardDescription>Configure the code editor behavior.</CardDescription>
            </CardHeader>
            <CardContent className='space-y-4'>
              <div className='flex items-center justify-between'>
                <div>
                  <p className='text-sm font-medium'>Auto Save</p>
                  <p className='text-xs text-muted-foreground'>Automatically save changes.</p>
                </div>
                <Switch checked={settings.autoSave} onCheckedChange={v => updateSetting('autoSave', v)} />
              </div>
              {settings.autoSave && (
                <div>
                  <label className='text-sm font-medium'>Auto Save Interval (seconds)</label>
                  <Input type='number' value={settings.autoSaveInterval} onChange={e => updateSetting('autoSaveInterval', e.target.value)} />
                </div>
              )}
              <Accordion type='single'>
                <AccordionItem value='keybindings'>
                  <AccordionTrigger value='keybindings'>Keyboard Shortcuts</AccordionTrigger>
                  <AccordionContent value='keybindings'>
                    <div className='space-y-2 text-sm'>
                      <div className='flex justify-between'>
                        <span>Save</span>
                        <kbd>Ctrl+S</kbd>
                      </div>
                      <div className='flex justify-between'>
                        <span>Find</span>
                        <kbd>Ctrl+F</kbd>
                      </div>
                      <div className='flex justify-between'>
                        <span>Replace</span>
                        <kbd>Ctrl+H</kbd>
                      </div>
                      <div className='flex justify-between'>
                        <span>Format</span>
                        <kbd>Shift+Alt+F</kbd>
                      </div>
                    </div>
                  </AccordionContent>
                </AccordionItem>
                <AccordionItem value='advanced'>
                  <AccordionTrigger value='advanced'>Advanced Options</AccordionTrigger>
                  <AccordionContent value='advanced'>
                    <p className='text-sm text-muted-foreground'>Advanced editor configuration options are available in the config file.</p>
                  </AccordionContent>
                </AccordionItem>
              </Accordion>
            </CardContent>
          </Card>
        </TabsContent>

        <TabsContent value='notifications'>
          <Card>
            <CardHeader>
              <CardTitle>Notification Preferences</CardTitle>
              <CardDescription>Choose how you want to be notified.</CardDescription>
            </CardHeader>
            <CardContent>
              <div>
                <label className='text-sm font-medium'>Email Digest</label>
                <Select value={settings.emailDigest} onValueChange={v => updateSetting('emailDigest', v)}>
                  <SelectTrigger>{settings.emailDigest}</SelectTrigger>
                  <SelectContent>
                    <SelectItem value='realtime'>Real-time</SelectItem>
                    <SelectItem value='daily'>Daily</SelectItem>
                    <SelectItem value='weekly'>Weekly</SelectItem>
                    <SelectItem value='off'>Off</SelectItem>
                  </SelectContent>
                </Select>
              </div>
            </CardContent>
          </Card>
        </TabsContent>

        <TabsContent value='security'>
          <Card>
            <CardHeader>
              <CardTitle>Security</CardTitle>
              <CardDescription>Manage your account security settings.</CardDescription>
            </CardHeader>
            <CardContent className='space-y-4'>
              <div className='flex items-center justify-between'>
                <div>
                  <p className='text-sm font-medium'>Two-Factor Authentication</p>
                  <p className='text-xs text-muted-foreground'>Add an extra layer of security.</p>
                </div>
                <Switch checked={settings.twoFactor} onCheckedChange={v => updateSetting('twoFactor', v)} />
              </div>
              <div>
                <label className='text-sm font-medium'>Session Timeout (minutes)</label>
                <Input type='number' value={settings.sessionTimeout} onChange={e => updateSetting('sessionTimeout', e.target.value)} />
              </div>
            </CardContent>
          </Card>
        </TabsContent>

        <TabsContent value='danger'>
          <Card>
            <CardHeader>
              <CardTitle>Danger Zone</CardTitle>
              <CardDescription>Irreversible and destructive actions.</CardDescription>
            </CardHeader>
            <CardContent className='space-y-4'>
              <div className='flex items-center justify-between rounded-lg border border-destructive p-4'>
                <div>
                  <p className='text-sm font-medium'>Delete Account</p>
                  <p className='text-xs text-muted-foreground'>Permanently delete your account and all data.</p>
                </div>
                <Dialog open={deleteDialogOpen} onOpenChange={setDeleteDialogOpen}>
                  <DialogTrigger>
                    <Button variant='destructive'>Delete Account</Button>
                  </DialogTrigger>
                  <DialogContent>
                    <DialogHeader>
                      <DialogTitle>Are you absolutely sure?</DialogTitle>
                    </DialogHeader>
                    <p className='text-sm text-muted-foreground'>This action cannot be undone. This will permanently delete your account.</p>
                    <DialogFooter>
                      <Button variant='outline' onClick={() => setDeleteDialogOpen(false)}>
                        Cancel
                      </Button>
                      <Button variant='destructive' onClick={handleDeleteAccount}>
                        Delete
                      </Button>
                    </DialogFooter>
                  </DialogContent>
                </Dialog>
              </div>
            </CardContent>
          </Card>
        </TabsContent>
      </Tabs>

      <div className='flex justify-end space-x-2'>
        <Button variant='outline' onClick={handleReset}>
          Reset to Defaults
        </Button>
        <Button onClick={handleSave}>Save All Settings</Button>
      </div>
    </div>
  );
}
