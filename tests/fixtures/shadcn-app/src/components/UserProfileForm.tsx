import { useState, useCallback, useMemo } from 'react';

import { Avatar } from './ui/Avatar';
import { Button } from './ui/Button';
import { Card, CardHeader, CardTitle, CardDescription, CardContent, CardFooter } from './ui/Card';
import { Input } from './ui/Input';
import { Select, SelectTrigger, SelectContent, SelectItem } from './ui/Select';
import { Switch } from './ui/Switch';
import { Textarea } from './ui/Textarea';
import { useToast } from './ui/Toast';

interface FormData {
  name: string;
  email: string;
  bio: string;
  role: string;
  notifications: boolean;
  newsletter: boolean;
}

const initialData: FormData = {
  name: '',
  email: '',
  bio: '',
  role: 'user',
  notifications: true,
  newsletter: false,
};

export function UserProfileForm() {
  const [data, setData] = useState<FormData>(initialData);
  const [errors, setErrors] = useState<Partial<Record<keyof FormData, string>>>({});
  const [saving, setSaving] = useState(false);
  const { addToast } = useToast();

  const updateField = useCallback(<K extends keyof FormData>(field: K, value: FormData[K]) => {
    setData(prev => ({ ...prev, [field]: value }));
    setErrors(prev => {
      const next = { ...prev };
      delete next[field];
      return next;
    });
  }, []);

  const validate = useCallback((): boolean => {
    const newErrors: Partial<Record<keyof FormData, string>> = {};
    if (!data.name.trim()) newErrors.name = 'Name is required';
    if (!data.email.trim()) newErrors.email = 'Email is required';
    else if (!/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(data.email)) newErrors.email = 'Invalid email';
    if (data.bio.length > 500) newErrors.bio = 'Bio must be under 500 characters';
    setErrors(newErrors);
    return Object.keys(newErrors).length === 0;
  }, [data]);

  const handleSubmit = useCallback(async () => {
    if (!validate()) return;
    setSaving(true);
    await new Promise(resolve => setTimeout(resolve, 1000));
    setSaving(false);
    addToast({ title: 'Profile updated', description: 'Your changes have been saved.' });
  }, [validate, addToast]);

  const handleReset = useCallback(() => {
    setData(initialData);
    setErrors({});
  }, []);

  const bioCharCount = useMemo(() => 500 - data.bio.length, [data.bio]);

  const initials = useMemo(
    () =>
      data.name
        .split(' ')
        .map(w => w[0])
        .join('')
        .toUpperCase()
        .slice(0, 2) || '??',
    [data.name],
  );

  return (
    <Card className='w-full max-w-2xl'>
      <CardHeader>
        <div className='flex items-center space-x-4'>
          <Avatar fallback={initials} size='lg' />
          <div>
            <CardTitle>Edit Profile</CardTitle>
            <CardDescription>Update your personal information and preferences.</CardDescription>
          </div>
        </div>
      </CardHeader>
      <CardContent className='space-y-6'>
        <div className='grid grid-cols-2 gap-4'>
          <div>
            <label className='text-sm font-medium'>Name</label>
            <Input value={data.name} onChange={e => updateField('name', e.target.value)} error={errors.name} placeholder='John Doe' />
          </div>
          <div>
            <label className='text-sm font-medium'>Email</label>
            <Input type='email' value={data.email} onChange={e => updateField('email', e.target.value)} error={errors.email} placeholder='john@example.com' />
          </div>
        </div>
        <div>
          <label className='text-sm font-medium'>Bio</label>
          <Textarea value={data.bio} onChange={e => updateField('bio', e.target.value)} error={errors.bio} placeholder='Tell us about yourself...' rows={4} />
          <p className='mt-1 text-xs text-muted-foreground'>{bioCharCount} characters remaining</p>
        </div>
        <div>
          <label className='text-sm font-medium'>Role</label>
          <Select value={data.role} onValueChange={v => updateField('role', v)}>
            <SelectTrigger>{data.role}</SelectTrigger>
            <SelectContent>
              <SelectItem value='user'>User</SelectItem>
              <SelectItem value='admin'>Admin</SelectItem>
              <SelectItem value='moderator'>Moderator</SelectItem>
              <SelectItem value='editor'>Editor</SelectItem>
            </SelectContent>
          </Select>
        </div>
        <div className='space-y-4'>
          <div className='flex items-center justify-between'>
            <div>
              <p className='text-sm font-medium'>Push Notifications</p>
              <p className='text-xs text-muted-foreground'>Receive notifications about activity.</p>
            </div>
            <Switch checked={data.notifications} onCheckedChange={v => updateField('notifications', v)} />
          </div>
          <div className='flex items-center justify-between'>
            <div>
              <p className='text-sm font-medium'>Newsletter</p>
              <p className='text-xs text-muted-foreground'>Get weekly updates via email.</p>
            </div>
            <Switch checked={data.newsletter} onCheckedChange={v => updateField('newsletter', v)} />
          </div>
        </div>
      </CardContent>
      <CardFooter className='flex justify-between'>
        <Button variant='outline' onClick={handleReset}>
          Reset
        </Button>
        <Button onClick={handleSubmit} disabled={saving}>
          {saving ? 'Saving...' : 'Save Changes'}
        </Button>
      </CardFooter>
    </Card>
  );
}
